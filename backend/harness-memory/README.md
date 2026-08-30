# harness-memory

The retrieval layer for the Rust agent harness. Builds, tests, and benchmarks clean
on stable Rust (verified on 1.75).

It is a member of the `backend/` Cargo workspace, so the repository gates cover
it: `npm run check:server` runs fmt, clippy and tests across both crates.

```
cargo test -p harness-memory --release     # 19 tests
cargo run -p harness-memory --release --bin bench
HARD=1 cargo run -p harness-memory --release --bin bench   # unstructured-noise worst case
```

## What this closes

Five rounds of the earlier drafts produced quantization, mmap storage, HNSW, SIMD
kernels, and a lock-free reload pipeline sitting on top of a retrieval layer that
could not return a symbol, a file path, or a line of source. The stack optimized a
path that never produced an answer. That is the gap this closes, plus the
correctness bugs that would have surfaced on first real use.

### The missing piece: row-aligned payloads

`store.rs` writes a vector *and* its payload under one row id. Four append-only
files, with the row count in the vectors header written **last**:

```
vectors.bin     [24B header][scale f32][norm f32][i8; D] x N
payload.idx     [16B header][offset u64][len u32][rsvd u32] x N
payload.dat     [16B header][bytes]
tombstones.log  [u32 row id] x M
```

Readers trust only the committed count, so a crash mid-append leaves an ignorable
tail rather than a torn row, and reopening rewinds all three write cursors to the
committed frontier. `uncommitted_tail_is_ignored_and_reclaimed` covers this.

Deletion appends to a tombstone log instead of flipping a flag in place —
nothing ever mutates bytes that a reader has mapped.

### Correctness fixes

| Problem | Fix |
|---|---|
| `create(truncate)` wiped the store on every startup | open-or-create; no truncate path exists |
| `reload()` compared file length, missing updates after orphan recovery | compares the committed count |
| Node id conflated with node slot (`nodes[row as usize]`) | explicit `HashMap<u32, usize>` slot map |
| `gen_range(0.0..1.0)` → `ln(0)` → `usize::MAX` level | samples `(0,1]`, clamped to `MAX_LEVEL` |
| `search_layer` unwrapped an empty result heap | returns empty on empty entry points |
| "nearest M" neighbour selection | paper's diversity heuristic with pruned-connection backfill |
| `partial_cmp().unwrap_or(Equal)` broke heap/sort invariants | `f32::total_cmp` |
| Query quantized before scoring | asymmetric f32×i8; query is never quantized |
| Symbol lookup keyed on the whole query string | tokenized (camelCase, snake_case, digits) |
| `+0.35` added to a cosine, compared against a flat `0.85` | reciprocal rank fusion over ranks |
| No invalidation — index served stale bodies after an edit | `index_file` tombstones the path's rows first |
| `embed_batch` defined but never called | batch is the primary method; `embed` defaults to it |
| Constant metadata preamble in embedded text | signature + doc + body only |
| Feature detection inside the innermost loop | resolved once into a `fn` pointer |
| SIMD kernels read `b` using `a.len()` | clamped to `min(len)`; dispatcher asserts equality |
| `HnswIndex<'a>` couldn't compose with an ingest worker | holds `Arc<Snapshot>` |
| `File::flush()` presented as durability | it is a no-op; `commit_durable` calls `sync_data` |

### Measured, not asserted

`bench.rs` warms both paths before timing either, and builds queries from many
different stored vectors. 20,000 × 384-dim, AVX2, single core:

| data | p50 exhaustive | p50 HNSW | speedup | recall@10 |
|---|---|---|---|---|
| clustered (40 clusters) | 3.5 ms | 119 µs | 29.8× | **1.000** |
| unstructured noise | 3.9 ms | 624 µs | 6.2× | **0.644** |

The second row is the honest worst case: uniform random vectors in 384 dimensions
have no manifold structure and distances concentrate, which defeats any graph
index. Real embeddings sit much closer to the first row. `ef_search` trades the
two monotonically, which is the check that the graph is sound rather than
merely fast:

```
ef_search   32 → recall 0.610 @ 337 µs
            64 → recall 0.644 @ 598 µs
           128 → recall 0.718 @ 1196 µs
           256 → recall 0.825 @ 2213 µs
           512 → recall 0.928 @ 3587 µs
```

Ground truth is computed against the same quantized records the index searches,
so recall isolates graph quality from quantization loss.

Storage: 392 B/row vs 1536 B/row as f32 (74.5% reduction).
Dot product: AVX2 34.7 ms vs scalar 178.4 ms over 2M iterations — **5.14×**, measured
with `black_box` on both inputs and the accumulator so LLVM cannot hoist the
loop-invariant computation.

## Layout

```
src/simd.rs     int8 kernels, one-time dispatch, length-checked
src/quant.rs    int8 format, asymmetric scoring, outlier clipping
src/store.rs    row-aligned storage, snapshots, ArcSwap reader
src/hnsw.rs     graph index over Arc<Snapshot>
src/embed.rs    Embedder trait (batch-first, sync)
src/memory.rs   CodebaseMemory: index, invalidate, hybrid query
src/bin/bench.rs
tests/integration.rs
```

Everything is synchronous by design. Embedding and graph insertion are CPU-bound;
`async fn` invites callers to run them on a reactor thread, which is the
starvation bug the earlier ingest worker had. Callers on tokio wrap these in
`spawn_blocking`.

## Not built yet

Deliberate omissions, roughly in priority order:

1. **Background ingest worker.** The API is sync and correct; wiring it to a
   dedicated OS thread with a channel is mechanical. Remap should be on a cadence
   (every N appends or ~50 ms), not per row.
2. **Segment compaction.** Tombstoned rows are never reclaimed. Compaction
   renumbers rows, which invalidates every graph edge, so it needs a rebuild —
   design it as segment rotation rather than in-place.
3. **f32 sidecar for rescoring.** Traverse on int8, rescore the top `ef` against
   full precision. Only ~`ef` rows are read per query, so it can live on disk.
4. **Graph persistence format.** `save`/`load` use JSON. Fine for correctness,
   roughly 5× larger than a packed binary at 20k nodes.
5. **Real embedder.** `HashEmbedder` is deterministic and similarity-bearing, so
   tests are not vacuous, and it is what the harness runs today — meaning
   `search_memory` matches on shared tokens, not on meaning. `fastembed` 4.x uses a
   builder (`InitOptions::new(model).with_show_download_progress(true)`), not the
   struct literal in the drafts, and the quantized MiniLM variant is
   `AllMiniLML6V2Q`.
6. **Concurrent inserts.** Single-writer today.

## Who reads it

The retrieval layer is no longer write-only. `backend/src/harness/` holds the
consumer: `memory.rs` cuts files into records and keeps the index current after
an edit, `tools.rs` registers `search_memory` so an agent can query it, and
`mod.rs` retrieves context before the first model turn. The items listed here
in earlier revisions as open outside this crate have been closed:

- **Provider layer.** `content_block_start` is handled, OpenAI payloads carry
  `ToolUse` and `ToolResult` blocks, and `tool_calls[i].index` groups parallel
  calls. `backend/tests/harness.rs` runs the same tool loop over all four
  protocols and asserts the result reaches the model in each dialect.
- **Compactor.** `backend/src/harness/transcript.rs` prunes whole turns rather
  than message indices, so a `tool_use` is never separated from its
  `tool_result`, and its summary is a system message that later passes update
  rather than discard.
- **Nothing reads this store.** `search_memory` is registered in the tool
  registry, and `Harness::retrieve_context` injects hits before the first turn.
