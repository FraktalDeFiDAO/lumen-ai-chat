use std::path::PathBuf;
use std::sync::Arc;

use harness_memory::embed::{Embedder, HashEmbedder};
use harness_memory::hnsw::{HnswConfig, HnswIndex};
use harness_memory::memory::{CodeSymbol, CodebaseMemory, MatchKind, Record};
use harness_memory::quant::{F32Query, QuantizedVector};
use harness_memory::store::{SegmentPaths, SegmentReader, SegmentWriter};

/// Scratch directory that cleans up on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hm-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn symbol(name: &str, file: &str, line: usize) -> CodeSymbol {
    CodeSymbol {
        name: name.to_string(),
        kind: "function".into(),
        signature: format!("pub fn {}()", name),
        doc: None,
        file_path: PathBuf::from(file),
        start_line: line,
        end_line: line + 10,
    }
}

// ============================================================ storage layer

#[test]
fn payload_survives_the_round_trip() {
    let tmp = Tmp::new("payload");
    let paths = SegmentPaths::in_dir(tmp.path());

    let mut w = SegmentWriter::open(&paths, 8).unwrap();
    for i in 0..5u32 {
        let v = QuantizedVector::quantize(&[i as f32; 8]);
        w.append(&v, format!("record-{}", i).as_bytes()).unwrap();
    }
    w.commit().unwrap();

    let reader = SegmentReader::open(paths).unwrap();
    let snap = reader.snapshot();
    assert_eq!(snap.len(), 5);
    for i in 0..5u32 {
        assert_eq!(snap.payload(i).unwrap(), format!("record-{}", i).as_bytes());
        assert_eq!(snap.vector(i).unwrap().bytes.len(), 8);
    }
}

/// The count header is the commit point: rows written but not committed must be
/// invisible, and the next open must reclaim their bytes.
#[test]
fn uncommitted_tail_is_ignored_and_reclaimed() {
    let tmp = Tmp::new("crash");
    let paths = SegmentPaths::in_dir(tmp.path());

    {
        let mut w = SegmentWriter::open(&paths, 4).unwrap();
        for i in 0..3u32 {
            w.append(&QuantizedVector::quantize(&[i as f32; 4]), b"committed")
                .unwrap();
        }
        w.commit().unwrap();
        // Written, never committed — the crash case.
        w.append(&QuantizedVector::quantize(&[99.0; 4]), b"orphan")
            .unwrap();
        w.append(&QuantizedVector::quantize(&[98.0; 4]), b"orphan")
            .unwrap();
    }

    let reader = SegmentReader::open(paths.clone()).unwrap();
    assert_eq!(
        reader.snapshot().len(),
        3,
        "orphan rows must not be visible"
    );

    // Reopening rewinds to the committed frontier, so the next row is 3.
    let mut w = SegmentWriter::open(&paths, 4).unwrap();
    assert_eq!(w.len(), 3);
    let row = w
        .append(&QuantizedVector::quantize(&[7.0; 4]), b"after-recovery")
        .unwrap();
    assert_eq!(row, 3);
    w.commit().unwrap();

    reader.reload().unwrap();
    let snap = reader.snapshot();
    assert_eq!(snap.len(), 4);
    assert_eq!(snap.payload(3).unwrap(), b"after-recovery");
    // Row 3's payload must be the recovered one, not the orphan's bytes.
    assert_ne!(snap.payload(3).unwrap(), b"orphan");
}

/// Reopening must never truncate. The earlier design called `create(truncate)`
/// on every startup and lost the whole store.
#[test]
fn reopening_preserves_existing_rows() {
    let tmp = Tmp::new("reopen");
    let paths = SegmentPaths::in_dir(tmp.path());

    {
        let mut w = SegmentWriter::open(&paths, 4).unwrap();
        w.append(&QuantizedVector::quantize(&[1.0; 4]), b"first")
            .unwrap();
        w.commit_durable().unwrap();
    }
    {
        let mut w = SegmentWriter::open(&paths, 4).unwrap();
        assert_eq!(w.len(), 1, "reopen must not truncate");
        w.append(&QuantizedVector::quantize(&[2.0; 4]), b"second")
            .unwrap();
        w.commit_durable().unwrap();
    }

    let snap = SegmentReader::open(paths).unwrap().snapshot();
    assert_eq!(snap.len(), 2);
    assert_eq!(snap.payload(0).unwrap(), b"first");
    assert_eq!(snap.payload(1).unwrap(), b"second");
}

#[test]
fn readers_see_appends_only_after_reload() {
    let tmp = Tmp::new("reload");
    let paths = SegmentPaths::in_dir(tmp.path());

    let mut w = SegmentWriter::open(&paths, 4).unwrap();
    w.append(&QuantizedVector::quantize(&[1.0; 4]), b"a")
        .unwrap();
    w.commit().unwrap();

    let reader = SegmentReader::open(paths).unwrap();
    let before = reader.snapshot();
    assert_eq!(before.len(), 1);

    w.append(&QuantizedVector::quantize(&[2.0; 4]), b"b")
        .unwrap();
    w.commit().unwrap();

    // The old snapshot is still valid and still shows the old view.
    assert_eq!(before.len(), 1);
    assert!(reader.reload().unwrap());
    assert_eq!(reader.snapshot().len(), 2);
    assert_eq!(before.len(), 1, "held snapshot must not shift underneath");
}

// =============================================================== hnsw layer

#[test]
fn hnsw_recall_against_brute_force() {
    let tmp = Tmp::new("recall");
    let paths = SegmentPaths::in_dir(tmp.path());
    let dim = 64;
    let n = 2000usize;

    // Clustered data — the case where "nearest M" neighbour selection degrades.
    let mut vectors = Vec::new();
    {
        let mut w = SegmentWriter::open(&paths, dim).unwrap();
        for i in 0..n {
            let cluster = (i % 20) as f32;
            let v: Vec<f32> = (0..dim)
                .map(|d| {
                    ((d as f32 * 0.1 + cluster * 3.0).sin())
                        + ((i as f32 * 0.001 + d as f32 * 0.7).cos() * 0.15)
                })
                .collect();
            w.append(&QuantizedVector::quantize(&v), b"{}").unwrap();
            vectors.push(v);
        }
        w.commit().unwrap();
    }

    let reader = SegmentReader::open(paths).unwrap();
    let snap = reader.snapshot();

    let mut index = HnswIndex::new_seeded(HnswConfig::default(), snap.clone(), 42);
    for row in 0..n as u32 {
        index.insert(row).unwrap();
    }
    assert_eq!(index.len(), n);

    // Recall@10 over many queries, not a single lucky one.
    let k = 10usize;
    let queries = 100usize;
    let mut hits = 0usize;

    for qi in 0..queries {
        let src = &vectors[(qi * 17) % n];
        let perturbed: Vec<f32> = src
            .iter()
            .enumerate()
            .map(|(d, x)| x + (d as f32 * 0.05 + qi as f32).sin() * 0.05)
            .collect();
        let query = F32Query::new(perturbed);

        let truth: Vec<u32> = snap
            .brute_force(&query, k)
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        let got: Vec<u32> = index
            .search(&query, k)
            .unwrap()
            .into_iter()
            .map(|(r, _)| r)
            .collect();

        hits += got.iter().filter(|r| truth.contains(r)).count();
    }

    let recall = hits as f64 / (queries * k) as f64;
    println!("recall@{} = {:.3} over {} queries", k, recall, queries);
    assert!(recall > 0.90, "recall@{} too low: {:.3}", k, recall);
}

#[test]
fn hnsw_handles_single_and_empty_index() {
    let tmp = Tmp::new("degenerate");
    let paths = SegmentPaths::in_dir(tmp.path());

    let mut w = SegmentWriter::open(&paths, 8).unwrap();
    w.commit().unwrap();
    let reader = SegmentReader::open(paths.clone()).unwrap();

    let empty = HnswIndex::new(HnswConfig::default(), reader.snapshot());
    let q = F32Query::new(vec![1.0f32; 8]);
    assert!(
        empty.search(&q, 5).is_err(),
        "empty index should report Empty"
    );

    w.append(&QuantizedVector::quantize(&[1.0; 8]), b"solo")
        .unwrap();
    w.commit().unwrap();
    reader.reload().unwrap();

    let mut one = HnswIndex::new(HnswConfig::default(), reader.snapshot());
    one.insert(0).unwrap();
    let found = one.search(&q, 5).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, 0);
}

/// Level sampling used to be able to produce `usize::MAX` and abort on
/// allocation. Hammer it.
#[test]
fn level_sampling_never_explodes() {
    let tmp = Tmp::new("levels");
    let paths = SegmentPaths::in_dir(tmp.path());
    let mut w = SegmentWriter::open(&paths, 8).unwrap();
    for i in 0..500u32 {
        w.append(
            &QuantizedVector::quantize(&[i as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]),
            b"x",
        )
        .unwrap();
    }
    w.commit().unwrap();

    let reader = SegmentReader::open(paths).unwrap();
    for seed in 0..20u64 {
        let mut idx = HnswIndex::new_seeded(HnswConfig::default(), reader.snapshot(), seed);
        for row in 0..500u32 {
            idx.insert(row).unwrap();
        }
        assert_eq!(idx.len(), 500);
    }
}

// ============================================================= memory layer

fn memory_fixture(tmp: &Tmp) -> CodebaseMemory {
    let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(256));
    CodebaseMemory::open(tmp.path(), embedder, HnswConfig::default()).unwrap()
}

#[test]
fn search_results_carry_their_symbol_and_content() {
    let tmp = Tmp::new("hits");
    let mem = memory_fixture(&tmp);

    mem.index_file(
        "src/agent/mod.rs",
        vec![
            Record {
                symbol: symbol("AgentEngine", "src/agent/mod.rs", 49),
                content: "the agent engine drives the completion and tool loop".into(),
            },
            Record {
                symbol: symbol("parse_config", "src/agent/mod.rs", 120),
                content: "reads toml configuration from disk".into(),
            },
        ],
    )
    .unwrap();

    let hits = mem.query("AgentEngine", 5).unwrap();
    assert!(!hits.is_empty(), "query returned nothing");

    let top = &hits[0];
    assert_eq!(top.record.symbol.name, "AgentEngine");
    assert_eq!(top.record.symbol.start_line, 49);
    assert_eq!(
        top.record.symbol.file_path,
        PathBuf::from("src/agent/mod.rs")
    );
    assert!(top.record.content.contains("tool loop"));
}

/// The old lookup keyed the symbol table on the entire lowercased query, so it
/// only ever fired for a bare identifier. A natural-language query containing an
/// identifier must still hit the exact path.
#[test]
fn symbol_lookup_fires_inside_a_natural_language_query() {
    let tmp = Tmp::new("tokens");
    let mem = memory_fixture(&tmp);

    let mut records = vec![Record {
        symbol: symbol("AgentEngine", "src/agent.rs", 10),
        content: "engine".into(),
    }];
    for i in 0..40 {
        records.push(Record {
            symbol: symbol(&format!("unrelated_{}", i), "src/agent.rs", 100 + i),
            content: format!("filler number {} about networking and sockets", i),
        });
    }
    mem.index_file("src/agent.rs", records).unwrap();

    let hits = mem
        .query("where does AgentEngine handle the execution loop", 5)
        .unwrap();

    let found = hits.iter().find(|h| h.record.symbol.name == "AgentEngine");
    assert!(
        found.is_some(),
        "identifier inside a sentence did not match"
    );
    let found = found.unwrap();
    assert!(
        matches!(found.kind, MatchKind::Symbol | MatchKind::Hybrid),
        "expected the exact-symbol path to contribute, got {:?}",
        found.kind
    );
}

/// The failure mode that matters most for a coding agent: it edits a file it
/// just indexed, and the index keeps serving the old bodies.
#[test]
fn reindexing_a_file_evicts_the_previous_version() {
    let tmp = Tmp::new("stale");
    let mem = memory_fixture(&tmp);

    mem.index_file(
        "src/handler.rs",
        vec![Record {
            symbol: symbol("handle_request", "src/handler.rs", 5),
            content: "OLD BODY returns a hardcoded placeholder response".into(),
        }],
    )
    .unwrap();

    mem.index_file(
        "src/handler.rs",
        vec![Record {
            symbol: symbol("handle_request", "src/handler.rs", 5),
            content: "NEW BODY validates the token then dispatches to the router".into(),
        }],
    )
    .unwrap();

    let hits = mem.query("handle_request", 10).unwrap();
    assert_eq!(hits.len(), 1, "stale row still being returned");
    assert!(hits[0].record.content.starts_with("NEW BODY"));
    assert_eq!(mem.live_count(), 1);
    assert_eq!(mem.len(), 2, "the old row should still exist, tombstoned");
}

#[test]
fn invalidated_rows_never_surface() {
    let tmp = Tmp::new("tomb");
    let mem = memory_fixture(&tmp);

    mem.index_file(
        "a.rs",
        vec![Record {
            symbol: symbol("alpha_function", "a.rs", 1),
            content: "alpha".into(),
        }],
    )
    .unwrap();
    mem.index_file(
        "b.rs",
        vec![Record {
            symbol: symbol("beta_function", "b.rs", 1),
            content: "beta".into(),
        }],
    )
    .unwrap();

    assert_eq!(mem.invalidate_file("a.rs").unwrap(), 1);

    let hits = mem.query("alpha_function", 10).unwrap();
    assert!(
        hits.iter()
            .all(|h| h.record.symbol.name != "alpha_function"),
        "tombstoned row was returned"
    );
    assert_eq!(mem.live_count(), 1);
}

/// Deletions must survive a restart, and the rebuilt index must agree.
#[test]
fn memory_state_survives_reopen() {
    let tmp = Tmp::new("persist");

    {
        let mem = memory_fixture(&tmp);
        mem.index_file(
            "keep.rs",
            vec![Record {
                symbol: symbol("kept_symbol", "keep.rs", 1),
                content: "this one stays".into(),
            }],
        )
        .unwrap();
        mem.index_file(
            "drop.rs",
            vec![Record {
                symbol: symbol("dropped_symbol", "drop.rs", 1),
                content: "this one goes".into(),
            }],
        )
        .unwrap();
        mem.invalidate_file("drop.rs").unwrap();
    }

    let mem = memory_fixture(&tmp);
    assert_eq!(mem.len(), 2);
    assert_eq!(mem.live_count(), 1, "tombstone did not survive restart");

    let hits = mem.query("kept_symbol", 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].record.symbol.name, "kept_symbol");

    let dropped = mem.query("dropped_symbol", 5).unwrap();
    assert!(dropped
        .iter()
        .all(|h| h.record.symbol.name != "dropped_symbol"));
}

#[test]
fn approximate_and_exhaustive_agree_at_small_scale() {
    let tmp = Tmp::new("agree");
    let mem = memory_fixture(&tmp);

    let records: Vec<Record> = (0..200)
        .map(|i| Record {
            symbol: symbol(&format!("func_{}", i), "big.rs", i),
            content: format!(
                "module {} handles {} for the {} subsystem",
                i % 7,
                ["parsing", "routing", "caching", "logging"][i % 4],
                ["network", "storage", "compute"][i % 3]
            ),
        })
        .collect();
    mem.index_file("big.rs", records).unwrap();

    let approx = mem
        .query_with("routing for the network subsystem", 5, true)
        .unwrap();
    let exact = mem
        .query_with("routing for the network subsystem", 5, false)
        .unwrap();

    assert_eq!(approx.len(), 5);
    let overlap = approx
        .iter()
        .filter(|a| exact.iter().any(|e| e.row == a.row))
        .count();
    assert!(
        overlap >= 4,
        "approximate diverged from exhaustive: {}/5",
        overlap
    );
}
