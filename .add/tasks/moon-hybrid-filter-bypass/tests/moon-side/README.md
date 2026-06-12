# Moon-side red tests — staged for the pilotspace/moon PR

These three test files belong in **`pilotspace/moon` → `tests/`** (sequencing
step 1 of TASK.md §3). They are staged here, not committed into `vendor/moon`,
because they reference symbols and a wire modifier that do not exist in the
pinned `vendor/moon` SHA (`HybridQuery.filter`, the `FILTER` FT.SEARCH clause —
CHANGE A/E). Compiling them against the current submodule would break
`cargo test` for the `moon` crate; they compile and run only after the Moon
implementation (CHANGE A–F) lands.

## Placement

Drop each file into the Moon repo's `tests/` directory alongside the existing
`tests/lunaris_hybrid_ft_search.rs`.

## Harness reuse (Moon's "duplicate, don't extract" test convention)

Each file depends on the per-file test harness already present in
`tests/lunaris_hybrid_ft_search.rs`:

- `build_config(port, num_shards) -> ServerConfig`
- `start_moon_sharded(num_shards) -> (u16, CancellationToken)`
- `connect(port) -> redis::aio::MultiplexedConnection`
- `vec4_bytes([f32;4]) -> Vec<u8>`
- `parse_search_keys(&redis::Value) -> (i64, Vec<String>)`

Copy those five helpers verbatim into each file (per Plan 165-03's
duplicate-not-extract convention), or factor them into a shared `mod`. The
test bodies below assume they are in scope.

## Expected RED reason (today, before the fix)

The new `FILTER` clause is unknown to Moon's HYBRID dispatcher
(`hybrid.rs::parse_hybrid_modifier` + `ft.rs`), so it is either rejected
(`ERR unsupported FILTER type`) or silently ignored — in the ignore case the
dense-KNN branch leaks the foreign source, exactly as the Lunaris-side suite
observes. After CHANGE A/B/E/F the foreign source is absent from BOTH branches.

## Feature gate

All three carry the same cfg gate as the sibling harness file:

```rust
#![cfg(all(feature = "runtime-tokio", feature = "text-index"))]
```

Run (matching the sibling file):

```sh
cargo test --test hybrid_filter_tag --no-default-features \
  --features runtime-tokio,jemalloc,graph,text-index -- --test-threads=1 --nocapture
```
