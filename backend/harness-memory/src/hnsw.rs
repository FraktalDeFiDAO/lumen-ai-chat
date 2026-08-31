//! HNSW graph over a `Snapshot`.
//!
//! Differences from the draft this replaces:
//!   * holds `Arc<Snapshot>` rather than `&'a Reader`, so it composes with a
//!     background ingest worker without a lifetime transmute;
//!   * node id and node slot are separate — traversal goes through a slot map
//!     instead of assuming row ids are dense and insertion-ordered;
//!   * level sampling can no longer produce `usize::MAX`;
//!   * neighbour selection uses the paper's diversity heuristic, not "nearest M";
//!   * `search_layer` tolerates empty entry points instead of unwrapping.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;
use std::sync::Arc;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::quant::{Int8Query, Query};
use crate::store::{Snapshot, StoreError};

/// Hard ceiling on node level. Without it a pathological sample allocates an
/// adjacency vector per level up to `usize::MAX`.
const MAX_LEVEL: usize = 16;

#[derive(Debug, Error)]
pub enum HnswError {
    #[error("storage: {0}")]
    Store(#[from] StoreError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("index is empty")]
    Empty,
    #[error("row {row} already present in graph")]
    Duplicate { row: u32 },
    #[error("graph references row {row} but the snapshot holds only {count} rows")]
    GraphAheadOfStore { row: u32, count: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswConfig {
    /// Max links per node above layer 0.
    pub m: usize,
    /// Max links per node on layer 0.
    pub m0: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
    /// Level decay factor; 1/ln(m) is the usual choice.
    pub ml: f64,
}

impl Default for HnswConfig {
    fn default() -> Self {
        let m = 16;
        Self {
            m,
            m0: m * 2,
            ef_construction: 100,
            ef_search: 64,
            ml: 1.0 / (m as f64).ln(),
        }
    }
}

#[derive(Copy, Clone, Debug)]
struct DistNode {
    distance: f32,
    row: u32,
}

impl PartialEq for DistNode {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for DistNode {}
impl Ord for DistNode {
    fn cmp(&self, other: &Self) -> Ordering {
        // total_cmp is a total order even across NaN, so the heap invariants and
        // sort_by contract hold no matter what the distance function returns.
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.row.cmp(&other.row))
    }
}
impl PartialOrd for DistNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Node {
    row: u32,
    level: usize,
    /// `neighbors[layer]` — adjacency by layer, index 0 is the base layer.
    neighbors: Vec<Vec<u32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswGraph {
    config: HnswConfig,
    entry: Option<u32>,
    max_layer: usize,
    nodes: Vec<Node>,
    /// row id -> index into `nodes`. Never assume these are equal.
    slot: HashMap<u32, usize>,
}

pub struct HnswIndex {
    graph: HnswGraph,
    snapshot: Arc<Snapshot>,
    rng: StdRng,
}

impl HnswIndex {
    pub fn new(config: HnswConfig, snapshot: Arc<Snapshot>) -> Self {
        Self {
            graph: HnswGraph {
                config,
                entry: None,
                max_layer: 0,
                nodes: Vec::new(),
                slot: HashMap::new(),
            },
            snapshot,
            rng: StdRng::from_os_rng(),
        }
    }

    /// Deterministic level sampling, for reproducible benchmarks and tests.
    pub fn new_seeded(config: HnswConfig, snapshot: Arc<Snapshot>, seed: u64) -> Self {
        let mut idx = Self::new(config, snapshot);
        idx.rng = StdRng::seed_from_u64(seed);
        idx
    }

    /// Point the index at a newer snapshot. Row ids are stable across snapshots
    /// because the store is append-only, so existing edges stay valid.
    pub fn update_snapshot(&mut self, snapshot: Arc<Snapshot>) {
        self.snapshot = snapshot;
    }

    pub fn len(&self) -> usize {
        self.graph.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.graph.nodes.is_empty()
    }

    pub fn config(&self) -> &HnswConfig {
        &self.graph.config
    }

    fn random_level(&mut self) -> usize {
        // random() yields [0,1); 1.0 - r moves it to (0,1] so ln() is always finite.
        let r: f64 = 1.0 - self.rng.random::<f64>();
        let lvl = (-r.ln() * self.graph.config.ml).floor();
        if !lvl.is_finite() || lvl < 0.0 {
            0
        } else {
            (lvl as usize).min(MAX_LEVEL)
        }
    }

    #[inline]
    fn node(&self, row: u32) -> Option<&Node> {
        self.graph.slot.get(&row).map(|&i| &self.graph.nodes[i])
    }

    #[inline]
    fn neighbors_at(&self, row: u32, layer: usize) -> &[u32] {
        match self.node(row) {
            Some(n) if layer < n.neighbors.len() => &n.neighbors[layer],
            _ => &[],
        }
    }

    #[inline]
    fn distance<Q: Query + ?Sized>(&self, q: &Q, row: u32) -> Result<f32, HnswError> {
        Ok(q.distance(&self.snapshot.vector(row)?))
    }

    /// Insert the vector stored at `row`.
    pub fn insert(&mut self, row: u32) -> Result<(), HnswError> {
        if self.graph.slot.contains_key(&row) {
            return Err(HnswError::Duplicate { row });
        }
        if row >= self.snapshot.len() {
            return Err(HnswError::GraphAheadOfStore {
                row,
                count: self.snapshot.len(),
            });
        }

        let level = self.random_level();
        // Own the key so the immutable borrow of the snapshot does not outlive
        // the mutable graph borrows below.
        let query = Int8Query(self.snapshot.vector(row)?.to_owned());

        let entry = match self.graph.entry {
            None => {
                self.push_node(row, level, vec![Vec::new(); level + 1]);
                self.graph.entry = Some(row);
                self.graph.max_layer = level;
                return Ok(());
            }
            Some(e) => e,
        };

        let top = self.graph.max_layer;
        let mut current = entry;
        let mut current_dist = self.distance(&query, current)?;

        // Phase 1: greedy descent through layers above the insertion level.
        for layer in (level + 1..=top).rev() {
            loop {
                let mut improved = false;
                let candidates: Vec<u32> = self.neighbors_at(current, layer).to_vec();
                for n in candidates {
                    let d = self.distance(&query, n)?;
                    if d < current_dist {
                        current_dist = d;
                        current = n;
                        improved = true;
                    }
                }
                if !improved {
                    break;
                }
            }
        }

        // Phase 2: connect on every layer at or below the insertion level.
        let mut entry_points = vec![current];
        let mut new_neighbors: Vec<Vec<u32>> = vec![Vec::new(); level + 1];

        for layer in (0..=level.min(top)).rev() {
            let candidates = self.search_layer(
                &query,
                &entry_points,
                self.graph.config.ef_construction,
                layer,
            )?;

            let max_edges = if layer == 0 {
                self.graph.config.m0
            } else {
                self.graph.config.m
            };

            let selected = self.select_neighbors(&candidates, max_edges)?;
            new_neighbors[layer] = selected.clone();

            for &neighbor in &selected {
                let over = {
                    let slot = self.graph.slot[&neighbor];
                    let node = &mut self.graph.nodes[slot];
                    if layer < node.neighbors.len() {
                        node.neighbors[layer].push(row);
                        node.neighbors[layer].len() > max_edges
                    } else {
                        false
                    }
                };
                if over {
                    let pruned = self.prune(neighbor, layer, max_edges)?;
                    let slot = self.graph.slot[&neighbor];
                    self.graph.nodes[slot].neighbors[layer] = pruned;
                }
            }

            entry_points = if candidates.is_empty() {
                entry_points
            } else {
                candidates.iter().map(|c| c.row).collect()
            };
        }

        self.push_node(row, level, new_neighbors);

        if level > top {
            self.graph.max_layer = level;
            self.graph.entry = Some(row);
        }
        Ok(())
    }

    fn push_node(&mut self, row: u32, level: usize, neighbors: Vec<Vec<u32>>) {
        let slot = self.graph.nodes.len();
        self.graph.nodes.push(Node {
            row,
            level,
            neighbors,
        });
        self.graph.slot.insert(row, slot);
    }

    fn search_layer<Q: Query + ?Sized>(
        &self,
        query: &Q,
        entry_points: &[u32],
        ef: usize,
        layer: usize,
    ) -> Result<Vec<DistNode>, HnswError> {
        if entry_points.is_empty() || ef == 0 {
            return Ok(Vec::new());
        }

        let mut visited: HashSet<u32> = HashSet::with_capacity(ef * 4);
        let mut candidates: BinaryHeap<Reverse<DistNode>> = BinaryHeap::new();
        let mut results: BinaryHeap<DistNode> = BinaryHeap::new();

        for &ep in entry_points {
            if !visited.insert(ep) || self.node(ep).is_none() {
                continue;
            }
            let node = DistNode {
                distance: self.distance(query, ep)?,
                row: ep,
            };
            candidates.push(Reverse(node));
            results.push(node);
        }
        while results.len() > ef {
            results.pop();
        }

        while let Some(Reverse(current)) = candidates.pop() {
            match results.peek() {
                Some(furthest) if current.distance > furthest.distance && results.len() >= ef => {
                    break
                }
                None => break,
                _ => {}
            }

            let neighbors: Vec<u32> = self.neighbors_at(current.row, layer).to_vec();
            for n in neighbors {
                if !visited.insert(n) {
                    continue;
                }
                if self.node(n).is_none() || n >= self.snapshot.len() {
                    continue;
                }
                let d = self.distance(query, n)?;
                let worst = results.peek().map(|f| f.distance).unwrap_or(f32::INFINITY);
                if d < worst || results.len() < ef {
                    let node = DistNode {
                        distance: d,
                        row: n,
                    };
                    candidates.push(Reverse(node));
                    results.push(node);
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }

        let mut out = results.into_vec();
        out.sort();
        Ok(out)
    }

    /// SELECT-NEIGHBORS-HEURISTIC from the HNSW paper, with pruned connections
    /// kept as backfill.
    ///
    /// A candidate is accepted only if it is closer to the new node than to any
    /// already-accepted neighbour. Taking the nearest M instead builds hubs on
    /// clustered data — and code embeddings cluster hard.
    fn select_neighbors(
        &self,
        candidates: &[DistNode],
        max_edges: usize,
    ) -> Result<Vec<u32>, HnswError> {
        let mut accepted: Vec<DistNode> = Vec::with_capacity(max_edges);
        let mut discarded: Vec<DistNode> = Vec::new();

        for &cand in candidates {
            if accepted.len() >= max_edges {
                break;
            }
            let cand_view = self.snapshot.vector(cand.row)?;
            let mut diverse = true;
            for existing in &accepted {
                let other = self.snapshot.vector(existing.row)?;
                let d_to_existing = 1.0 - cand_view.cosine(&other);
                if d_to_existing < cand.distance {
                    diverse = false;
                    break;
                }
            }
            if diverse {
                accepted.push(cand);
            } else {
                discarded.push(cand);
            }
        }

        for d in discarded {
            if accepted.len() >= max_edges {
                break;
            }
            accepted.push(d);
        }

        Ok(accepted.into_iter().map(|n| n.row).collect())
    }

    fn prune(&self, row: u32, layer: usize, max_edges: usize) -> Result<Vec<u32>, HnswError> {
        let anchor = Int8Query(self.snapshot.vector(row)?.to_owned());
        let mut scored: Vec<DistNode> = Vec::new();
        for &n in self.neighbors_at(row, layer) {
            scored.push(DistNode {
                distance: self.distance(&anchor, n)?,
                row: n,
            });
        }
        scored.sort();
        self.select_neighbors(&scored, max_edges)
    }

    /// Top-k search. Tombstoned rows are traversed (they carry connectivity) but
    /// never returned.
    pub fn search<Q: Query + ?Sized>(
        &self,
        query: &Q,
        k: usize,
    ) -> Result<Vec<(u32, f32)>, HnswError> {
        let entry = self.graph.entry.ok_or(HnswError::Empty)?;
        if k == 0 {
            return Ok(Vec::new());
        }

        let mut current = entry;
        let mut current_dist = self.distance(query, current)?;

        for layer in (1..=self.graph.max_layer).rev() {
            loop {
                let mut improved = false;
                let neighbors: Vec<u32> = self.neighbors_at(current, layer).to_vec();
                for n in neighbors {
                    let d = self.distance(query, n)?;
                    if d < current_dist {
                        current_dist = d;
                        current = n;
                        improved = true;
                    }
                }
                if !improved {
                    break;
                }
            }
        }

        // Over-fetch so deletions do not silently shrink the result set.
        let ef = self
            .graph
            .config
            .ef_search
            .max(k * 2)
            .max(k + self.snapshot.deleted_count().min(64));

        let found = self.search_layer(query, &[current], ef, 0)?;

        Ok(found
            .into_iter()
            .filter(|n| !self.snapshot.is_deleted(n.row))
            .take(k)
            .map(|n| (n.row, 1.0 - n.distance))
            .collect())
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), HnswError> {
        let file = File::create(path)?;
        serde_json::to_writer(BufWriter::new(file), &self.graph)?;
        Ok(())
    }

    /// Load a persisted graph and bind it to a snapshot.
    ///
    /// Rejects a graph that references rows the store does not have, which is
    /// what a stale graph paired with a rebuilt vector file looks like.
    pub fn load(path: impl AsRef<Path>, snapshot: Arc<Snapshot>) -> Result<Self, HnswError> {
        let file = File::open(path)?;
        let graph: HnswGraph = serde_json::from_reader(BufReader::new(file))?;

        for node in &graph.nodes {
            if node.row >= snapshot.len() {
                return Err(HnswError::GraphAheadOfStore {
                    row: node.row,
                    count: snapshot.len(),
                });
            }
        }

        Ok(Self {
            graph,
            snapshot,
            rng: StdRng::from_os_rng(),
        })
    }

    /// Rows present in the store but not yet in the graph. Used on restart to
    /// repair rows that were committed before an ingest crash.
    pub fn missing_rows(&self) -> Vec<u32> {
        (0..self.snapshot.len())
            .filter(|r| !self.graph.slot.contains_key(r))
            .collect()
    }
}
