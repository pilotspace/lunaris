# BUILD-CONTEXT — Moon-side implementation (moon-hybrid-filter-bypass)

Shared context for the Moon-side BUILD (sequencing step 1: pilotspace/moon PR).
Authoritative contract: `TASK.md` §3 (FROZEN v1.1). This file adds the
**verified-against-HEAD code map + API facts** so you do NOT re-discover them.

## Where you work

- Repo: **`/Volumes/Games/tindang-repo/moon`** (the pilotspace/moon clone,
  HEAD `7d2f271`). NOT the lunaris repo. All edits land here.
- The lunaris repo's `vendor/moon` submodule is bumped LATER (step 2) — do not
  touch it.

## Goal (the only success signal)

Make a pushed-down filter constrain BOTH branches of Moon's native HYBRID
FT.SEARCH (BM25 + dense-KNN + optional sparse) before RRF fusion, on the
single-shard AND multi-shard paths, plus an SDK param. Evidence = the 3 staged
Moon tests go green and the existing hybrid/FT suite stays green.

## TDD protocol (red → green; ADD non-negotiable)

1. Copy the 3 staged tests from
   `/Volumes/Games/tindang-repo/lunaris/.add/tasks/moon-hybrid-filter-bypass/tests/moon-side/`
   into `tests/` of THIS repo. They currently have placeholder comments for the
   harness helpers — copy `build_config` / `start_moon_sharded` / `connect` /
   `vec4_bytes` / `parse_search_keys` verbatim from
   `tests/lunaris_hybrid_ft_search.rs` into each (Moon's duplicate-don't-extract
   convention), or factor a shared `mod`.
2. Run them FIRST and confirm RED for the right reason (FILTER clause unknown /
   foreign source leaks), not a broken harness.
3. Implement CHANGE A–G. Re-run to green. Never weaken a test to pass.

Build/run (matches the sibling harness file's gate):
```sh
cargo test --test hybrid_filter_tag --test hybrid_filter_multishard \
  --test hybrid_filter_backward_compat \
  --no-default-features --features runtime-tokio,jemalloc,graph,text-index \
  -- --test-threads=1 --nocapture
```
Also keep green: `cargo test --test lunaris_hybrid_ft_search` and the wider
`vector_search` unit tests.

## Verified code map (HEAD 7d2f271 — citations confirmed)

### `src/command/vector_search/hybrid.rs` (1257 lines — watch the 1500-line cap)
- `pub struct HybridQuery` @52 — fields: index_name, text_query, dense_field,
  dense_blob, sparse: Option<(Bytes,Bytes)>, weights:[f32;3],
  k_per_stream:Option<usize>, top_k, offset, count. **ADD `pub filter:
  Option<HybridFilter>` (CHANGE A).**
- `impl HybridQuery::effective_k_per_stream` @78 — default
  `max(60, 3*top_k)`. **CHANGE C: when `self.filter.is_some()`, fan out to
  `max(60, 5*top_k)` (≥ contract's `max(default, 5*top_k, 60)`).**
- `struct HybridQueryPartial` @89 + `pub fn parse_hybrid_modifier` @111 — the
  HYBRID-clause parser. It scans VECTOR/SPARSE/FUSION/WEIGHTS/K_PER_STREAM and
  returns `end_index`. **CHANGE E: after K_PER_STREAM, parse an optional
  `FILTER <expr>` clause** (recursive prefix encoding below). Add
  `filter: Option<HybridFilter>` to `HybridQueryPartial`, advance `end_index`
  past it. Helpers `matches_keyword`, `extract_bulk`, `extract_field_token`,
  `parse_usize`, `parse_f32` already exist in-file.
- `pub fn execute_hybrid_search_local` @300 — single-shard executor. Flow:
  - BM25 → `text_results: Vec<TextSearchResult>` (@343, via
    `ft_text_search::execute_query_on_index_as_of`), then
    `bm25_to_search_results(&text_results)` @352.
  - Dense → `run_dense_knn(...) -> (dense_results: Vec<SearchResult>,
    key_hash_to_key: HashMap<u64,Bytes>)` @359.
  - Sparse (optional) → `sparse_results: Vec<SearchResult>` @372.
  - `rrf_fuse_three(&bm25, &dense, &sparse, weights, top_k)` @393.
  **CHANGE B: after the three streams are collected and BEFORE
  `rrf_fuse_three`, if `query.filter.is_some()`, compute a doc_id allowlist
  from `text_index` and retain only matching results in each stream.**
- `pub(super) fn run_dense_knn` @417.

### `SearchResult` / `TextSearchResult` identity (the join key)
- `TextSearchResult { doc_id: u32, key: Bytes, ... }` (text/store.rs:42).
- `SearchResult { distance, id: VectorId, key_hash: u64 }`. BM25→SearchResult
  sets `key_hash = xxh64(key, 0)`; dense/sparse `key_hash` populated at index
  time with the SAME seed. **`key_hash` is the cross-stream identity RRF dedups
  on.**

### `TextIndex` allowlist API (`src/text/store.rs`)
- `pub fn search_tag(&self, field:&Bytes, value:&Bytes) -> Vec<u32>` @1026 —
  **EXACT match only** (looks up `tag_indexes[field][value]` bitmap). Returns
  doc_ids. For **StartsWith/prefix** (`{prefix*}` — value ends in `*`): scan
  `tag_indexes[field]` keys for the prefix and UNION their bitmaps (write a
  `search_tag_prefix` helper; this is allowlist-build, not dispatch hot-path).
- `pub fn search_numeric_range(...)` @1205 — read its exact signature; returns
  doc_ids for a `[min,max]` range on a NUMERIC field. Use for the Numeric leaf.
- Maps on `TextIndex`: `key_hash_to_doc_id: HashMap<u64,u32>` @80,
  `doc_id_to_key: HashMap<u32,Bytes>` @82.

### Filter evaluation (CHANGE B core)
Define on the Moon side:
```rust
pub enum HybridFilter {
    Tag { field: String, value: String },     // value ending in '*' ⇒ prefix
    Numeric { field: String, min: f64, max: f64 },
    And(Vec<HybridFilter>),
    Or(Vec<HybridFilter>),
}
```
`fn eval_filter(f:&HybridFilter, ix:&TextIndex) -> std::collections::HashSet<u32>`:
- Tag (exact) → `search_tag(field, value)` into a set.
- Tag (prefix, trailing `*`) → `search_tag_prefix(field, prefix)`.
- Numeric → `search_numeric_range(...)` into a set.
- And → fold intersection; Or → fold union. Evaluate bottom-up.
Then filter streams:
- BM25: retain `text_results` where `doc_id ∈ allow` (filter BEFORE
  `bm25_to_search_results`).
- Dense/Sparse: retain results where
  `text_index.key_hash_to_doc_id.get(&r.key_hash)` is `Some(id)` AND
  `id ∈ allow`. **A result whose key_hash has NO text doc_id is DROPPED**
  (correctness-first: we cannot confirm it matches an indexed-field filter; a
  filtered BM25-only search would not see it either, so this matches the
  k-starvation baseline). Document this.
Tree depth/width bound (parser-enforced, CHANGE E): max depth 4, max 16 leaves
→ `Frame::Error("ERR FILTER too complex")`.

### CHANGE E — `FILTER` wire grammar (recursive prefix, arity-counted)
Appended after the FUSION/WEIGHTS/K_PER_STREAM block:
```
FILTER <expr>
<expr> := TAG @<field> <value>
        | NUMERIC @<field> <min> <max>
        | AND <n> <expr>{n}
        | OR  <n> <expr>{n}
```
- `TAG @source scratchpad`, `AND 2 TAG @source a NUMERIC @valid_time 0 99`.
- Unknown head → `Frame::Error("ERR unsupported FILTER type")`.
- depth>4 or leaves>16 → `Frame::Error("ERR FILTER too complex")`.
- Parser MUST NOT panic on truncation/overflow/garbage — return `Frame::Error`.
  Add unit tests for truncated/over-deep/over-wide/garbage inputs (the
  contract's least-sure flag: hand-rolled handler parsers have no fuzz cover).
- Absent FILTER ⇒ `filter = None` (backward compat — the
  hybrid_filter_backward_compat test pins this).

### CHANGE D — dispatcher
Find where `HybridQuery` is constructed from `HybridQueryPartial` (the FT.SEARCH
dispatcher — `src/server/conn/handler_monoio/ft.rs` and/or
`src/command/vector_search/ft_search/…`). Thread `partial.filter` into
`HybridQuery { …, filter }`.

### CHANGE F — multi-shard (the F2 discriminator; the v1.0 draft MISSED this)
- `src/shard/scatter_hybrid.rs` (655 lines): `scatter_hybrid_search`.
  `num_shards==1` → `execute_hybrid_search_local` (covered by B). `num_shards>1`
  → per-shard raw-streams fan-out via
  `src/command/vector_search/hybrid_multi.rs::execute_hybrid_search_local_raw_streams`
  then coordinator `rrf_fuse_three`.
- `FtHybridPayload` (in `src/shard/dispatch.rs`, ~222-240): the in-process
  per-shard request. **ADD `pub filter: Option<HybridFilter>`**;
  `scatter_hybrid_search` copies it from `HybridQuery` into each shard payload.
- `hybrid_multi.rs::execute_hybrid_search_local_raw_streams`: compute the
  allowlist from ITS OWN shard-local `text_index` (doc_ids are shard-local — the
  allowlist CANNOT be computed at the coordinator) and apply it to all three
  raw streams BEFORE returning them. Filtering MUST happen per-shard
  pre-return, NOT after coordinator `rrf_fuse_three` (post-fusion filtering
  reintroduces k-starvation). Apply the CHANGE C k_per_stream fan-out here too.

### CHANGE G — Rust SDK (`sdk/rust/src/text.rs`)
- `hybrid_search(...)` (~line 48): add a trailing `filter: Option<HybridFilter>`
  param. Mirror the server `HybridFilter` enum in the SDK. Encode it as the
  CHANGE E wire form appended after FUSION/WEIGHTS. `None` ⇒ wire unchanged
  (backward compat). Enforce the same depth/leaf limits client-side and return
  an SDK error before sending.

## Hard constraints
- No `.rs` file > 1500 lines (hybrid.rs is at 1257 — if CHANGE A/B/E push it
  over, split filter types/eval/parser into a sibling module e.g.
  `src/command/vector_search/hybrid_filter.rs`).
- No lock held across `.await`. No `std::sync::*Lock` (use parking_lot).
- The recursive parser must be panic-free on all inputs.
- Backward compat: `filter=None` ⇒ byte-identical behaviour to today (pinned by
  hybrid_filter_backward_compat + lunaris_hybrid_ft_search staying green).
- Do NOT change the staged tests' assertions to pass. Commit incrementally with
  the repo's `git commit -F tmp/<msg>.txt` convention (author: Tin Dang).

## Out of scope (named, do not implement)
- FT.NAVIGATE filter gap (separate task `ft-navigate-filter-gap`).
- Ingest `valid_time_ms` population (separate follow-up; tests direct-write it).
- The lunaris-side CHANGE H/I/J/K (lands in the lunaris repo AFTER the submodule
  bump).
