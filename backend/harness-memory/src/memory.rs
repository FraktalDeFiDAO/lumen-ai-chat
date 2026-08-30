//! The retrieval layer: what the storage and index below it are actually for.
//!
//! Fixes carried over from review of the earlier draft:
//!   * search results carry their symbol and content, because rows now have
//!     payloads;
//!   * the exact-symbol lookup keys on *tokens from the query*, not on the whole
//!     query string — the old `symbol_table.get(&query.to_lowercase())` only ever
//!     hit when the query was a bare identifier and nothing else;
//!   * vector and symbol hits are fused with reciprocal rank fusion instead of
//!     adding `+0.35` to a cosine score and comparing it against a flat `0.85`;
//!   * `index_file` invalidates a path's old rows first, so editing a file does
//!     not leave the previous version of every symbol in the index — the failure
//!     mode that matters most for a coding agent, which mutates the files it just
//!     indexed;
//!   * embeddings go through `embed_batch`;
//!   * the embedded text carries no constant metadata preamble, which would push
//!     every vector toward a common centroid and compress score dynamic range.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::embed::{EmbedError, Embedder};
use crate::hnsw::{HnswConfig, HnswError, HnswIndex};
use crate::quant::{F32Query, QuantizedVector};
use crate::store::{SegmentPaths, SegmentReader, SegmentWriter, StoreError};

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("storage: {0}")]
    Store(#[from] StoreError),
    #[error("index: {0}")]
    Hnsw(#[from] HnswError),
    #[error("embedding: {0}")]
    Embed(#[from] EmbedError),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("lock poisoned")]
    Poisoned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodeSymbol {
    pub name: String,
    pub kind: String,
    pub signature: String,
    pub doc: Option<String>,
    pub file_path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
}

/// What gets stored in a row's payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub symbol: CodeSymbol,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchKind {
    /// Retrieved by embedding similarity only.
    Vector,
    /// Retrieved by exact symbol-name token match only.
    Symbol,
    /// Retrieved by both.
    Hybrid,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub row: u32,
    pub record: Record,
    /// Fused rank score. Comparable across hits within one query, not across
    /// queries, and deliberately not a cosine value.
    pub score: f32,
    pub kind: MatchKind,
    /// Cosine similarity, when this hit came through the vector path.
    pub similarity: Option<f32>,
}

/// Splits identifiers into searchable tokens: delimiters, camelCase, and
/// digit boundaries. `parse_HTTPResponse_v2` yields the whole thing plus
/// `parse`, `http`, `response`, `v`, `2`.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let lower = raw.to_lowercase();
        if lower.len() > 1 {
            out.push(lower.clone());
        }

        // camelCase / PascalCase / acronym boundaries
        let chars: Vec<char> = raw.chars().collect();
        let mut start = 0usize;
        for i in 1..=chars.len() {
            let boundary = if i == chars.len() {
                true
            } else {
                let prev = chars[i - 1];
                let cur = chars[i];
                (prev.is_lowercase() && cur.is_uppercase())
                    || (prev.is_alphabetic() && cur.is_numeric())
                    || (prev.is_numeric() && cur.is_alphabetic())
                    || (prev.is_uppercase()
                        && cur.is_lowercase()
                        && i >= 2
                        && chars[i - 2].is_uppercase())
            };
            if boundary && i > start {
                let piece: String = chars[start..i].iter().collect::<String>().to_lowercase();
                if piece.len() > 1 && piece != lower {
                    out.push(piece);
                }
                start = i;
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

pub struct CodebaseMemory {
    embedder: Arc<dyn Embedder>,
    writer: Mutex<SegmentWriter>,
    reader: Arc<SegmentReader>,
    index: RwLock<HnswIndex>,
    /// token -> rows whose symbol name contains that token.
    symbol_tokens: RwLock<HashMap<String, Vec<u32>>>,
    /// file -> rows, so a changed file can invalidate exactly its own rows.
    by_file: RwLock<HashMap<PathBuf, Vec<u32>>>,
}

impl CodebaseMemory {
    pub fn open(
        dir: impl AsRef<Path>,
        embedder: Arc<dyn Embedder>,
        config: HnswConfig,
    ) -> Result<Self, MemoryError> {
        let paths = SegmentPaths::in_dir(&dir);
        let dim = embedder.dimension();

        let mut writer = SegmentWriter::open(&paths, dim)?;
        writer.commit()?;
        let reader = SegmentReader::open(paths)?;
        let snapshot = reader.snapshot();

        let mut index = HnswIndex::new_seeded(config, snapshot.clone(), 0x5eed);
        let mut symbol_tokens: HashMap<String, Vec<u32>> = HashMap::new();
        let mut by_file: HashMap<PathBuf, Vec<u32>> = HashMap::new();

        // Rebuild the in-memory maps and repair any rows committed before a crash.
        for row in 0..snapshot.len() {
            let bytes = snapshot.payload(row)?;
            if let Ok(rec) = serde_json::from_slice::<Record>(bytes) {
                for tok in tokenize(&rec.symbol.name) {
                    symbol_tokens.entry(tok).or_default().push(row);
                }
                by_file
                    .entry(rec.symbol.file_path.clone())
                    .or_default()
                    .push(row);
            }
            if !snapshot.is_deleted(row) {
                index.insert(row)?;
            }
        }

        Ok(Self {
            embedder,
            writer: Mutex::new(writer),
            reader,
            index: RwLock::new(index),
            symbol_tokens: RwLock::new(symbol_tokens),
            by_file: RwLock::new(by_file),
        })
    }

    pub fn len(&self) -> u32 {
        self.reader.snapshot().len()
    }

    /// Whether any row has ever been written, tombstoned rows included.
    /// [`live_count`](Self::live_count) is the one to ask before searching.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn live_count(&self) -> usize {
        self.reader.snapshot().live_count()
    }

    /// Text handed to the embedder.
    ///
    /// Signature, doc, and body only. The old version prefixed every record with
    /// `"Context Memory Turn\nSession: ...\nRole: ..."`, which is near-identical
    /// across records and drags all the vectors toward one point.
    fn embed_text(&self, rec: &Record) -> String {
        let mut s = String::new();
        s.push_str(&rec.symbol.signature);
        if let Some(doc) = &rec.symbol.doc {
            s.push('\n');
            s.push_str(doc);
        }
        s.push('\n');
        s.push_str(&rec.content);
        s.truncate(self.embedder.max_input_chars());
        s
    }

    /// Replace everything indexed for one file.
    ///
    /// Old rows are tombstoned before the new ones land, which is what keeps the
    /// index honest when an agent edits a file it previously indexed.
    pub fn index_file(
        &self,
        path: impl AsRef<Path>,
        records: Vec<Record>,
    ) -> Result<Vec<u32>, MemoryError> {
        let path = path.as_ref().to_path_buf();
        self.invalidate_file(&path)?;

        if records.is_empty() {
            return Ok(Vec::new());
        }

        let texts: Vec<String> = records.iter().map(|r| self.embed_text(r)).collect();
        let embeddings = self.embedder.embed_batch(&texts)?;

        let mut rows = Vec::with_capacity(records.len());
        {
            let mut writer = self.writer.lock().map_err(|_| MemoryError::Poisoned)?;
            for (rec, emb) in records.iter().zip(embeddings.iter()) {
                let qv = QuantizedVector::quantize_clipped(emb, 0.995);
                let payload = serde_json::to_vec(rec)?;
                rows.push(writer.append(&qv, &payload)?);
            }
            writer.commit()?;
        }

        // One remap for the whole batch, not one per row.
        self.reader.reload()?;
        let snapshot = self.reader.snapshot();

        {
            let mut index = self.index.write().map_err(|_| MemoryError::Poisoned)?;
            index.update_snapshot(snapshot);
            for &row in &rows {
                index.insert(row)?;
            }
        }

        {
            let mut tokens = self
                .symbol_tokens
                .write()
                .map_err(|_| MemoryError::Poisoned)?;
            let mut by_file = self.by_file.write().map_err(|_| MemoryError::Poisoned)?;
            for (rec, &row) in records.iter().zip(rows.iter()) {
                for tok in tokenize(&rec.symbol.name) {
                    tokens.entry(tok).or_default().push(row);
                }
                by_file.entry(path.clone()).or_default().push(row);
            }
        }

        Ok(rows)
    }

    /// Tombstone every row belonging to a path.
    pub fn invalidate_file(&self, path: impl AsRef<Path>) -> Result<usize, MemoryError> {
        let path = path.as_ref().to_path_buf();
        let rows = {
            let mut by_file = self.by_file.write().map_err(|_| MemoryError::Poisoned)?;
            by_file.remove(&path).unwrap_or_default()
        };
        if rows.is_empty() {
            return Ok(0);
        }

        {
            let mut writer = self.writer.lock().map_err(|_| MemoryError::Poisoned)?;
            for &row in &rows {
                writer.tombstone(row)?;
            }
        }

        {
            let mut tokens = self
                .symbol_tokens
                .write()
                .map_err(|_| MemoryError::Poisoned)?;
            for list in tokens.values_mut() {
                list.retain(|r| !rows.contains(r));
            }
            tokens.retain(|_, v| !v.is_empty());
        }

        self.reader.reload()?;
        let snapshot = self.reader.snapshot();
        self.index
            .write()
            .map_err(|_| MemoryError::Poisoned)?
            .update_snapshot(snapshot);

        Ok(rows.len())
    }

    /// Hybrid retrieval: approximate vector search fused with exact symbol-token
    /// lookup.
    pub fn query(&self, text: &str, k: usize) -> Result<Vec<SearchHit>, MemoryError> {
        self.query_with(text, k, true)
    }

    /// `approximate = false` scans every row. Use it as ground truth.
    pub fn query_with(
        &self,
        text: &str,
        k: usize,
        approximate: bool,
    ) -> Result<Vec<SearchHit>, MemoryError> {
        let snapshot = self.reader.snapshot();
        if snapshot.live_count() == 0 || k == 0 {
            return Ok(Vec::new());
        }

        let embedding = self.embedder.embed(text)?;
        let query = F32Query::new(embedding);
        let fetch = (k * 3).max(k + 10);

        let vector_hits: Vec<(u32, f32)> = if approximate {
            let index = self.index.read().map_err(|_| MemoryError::Poisoned)?;
            match index.search(&query, fetch) {
                Ok(v) => v,
                Err(HnswError::Empty) => Vec::new(),
                Err(e) => return Err(e.into()),
            }
        } else {
            snapshot.brute_force(&query, fetch)
        };

        // Exact path: every token of the query, not the query string as one key.
        let symbol_hits: Vec<u32> = {
            let tokens = self
                .symbol_tokens
                .read()
                .map_err(|_| MemoryError::Poisoned)?;
            let mut counts: HashMap<u32, usize> = HashMap::new();
            for tok in tokenize(text) {
                if let Some(rows) = tokens.get(&tok) {
                    for &row in rows {
                        if !snapshot.is_deleted(row) {
                            *counts.entry(row).or_insert(0) += 1;
                        }
                    }
                }
            }
            let mut ranked: Vec<(u32, usize)> = counts.into_iter().collect();
            // More matched tokens ranks higher; ties broken by row for determinism.
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            ranked.into_iter().take(fetch).map(|(r, _)| r).collect()
        };

        // Reciprocal rank fusion. Combining a cosine score with a hand-tuned
        // constant compares two different scales; RRF combines ranks, which is
        // scale-free and needs no tuning.
        const RRF_K: f32 = 60.0;
        let mut fused: HashMap<u32, (f32, bool, bool, Option<f32>)> = HashMap::new();

        for (rank, (row, sim)) in vector_hits.iter().enumerate() {
            let e = fused.entry(*row).or_insert((0.0, false, false, None));
            e.0 += 1.0 / (RRF_K + rank as f32 + 1.0);
            e.1 = true;
            e.3 = Some(*sim);
        }
        for (rank, row) in symbol_hits.iter().enumerate() {
            let e = fused.entry(*row).or_insert((0.0, false, false, None));
            e.0 += 1.0 / (RRF_K + rank as f32 + 1.0);
            e.2 = true;
        }

        let mut ranked: Vec<(u32, f32, MatchKind, Option<f32>)> = fused
            .into_iter()
            .map(|(row, (score, vec_hit, sym_hit, sim))| {
                let kind = match (vec_hit, sym_hit) {
                    (true, true) => MatchKind::Hybrid,
                    (false, true) => MatchKind::Symbol,
                    _ => MatchKind::Vector,
                };
                (row, score, kind, sim)
            })
            .collect();

        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });

        let mut out = Vec::with_capacity(k);
        for (row, score, kind, sim) in ranked.into_iter() {
            if out.len() >= k {
                break;
            }
            if snapshot.is_deleted(row) {
                continue;
            }
            let record: Record = serde_json::from_slice(snapshot.payload(row)?)?;
            out.push(SearchHit {
                row,
                record,
                score,
                kind,
                similarity: sim,
            });
        }
        Ok(out)
    }

    /// Persist the graph topology.
    pub fn save_index(&self, path: impl AsRef<Path>) -> Result<(), MemoryError> {
        self.index
            .read()
            .map_err(|_| MemoryError::Poisoned)?
            .save(path)?;
        Ok(())
    }
}
