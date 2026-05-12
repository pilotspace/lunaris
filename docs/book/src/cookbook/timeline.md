# Timeline Reconstruction

**Reach for `TimelineReconstruction` when you have a stream of dated events
and want to stitch a narrative for a time window — "what happened between
Jan 10 and Jan 16" or "what did the timeline look like as of Jan 13".**

`TimelineReconstruction`
(`crates/lunaris-recipes/src/documentary/timeline_reconstruction.rs`) is a
deliberately thin two-call composition of
[`DocumentCorpus`](./index.md#documentcorpus--hybrid-vector--keyword-rag)
(ingest) and [`TemporalQuery<Documents>`](./index.md#temporalquerys--typestate-time-travel)
(recall). Its value is discoverability as a named recipe, not code volume.

| Method | Signature | Notes |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>, source_prefix: impl Into<String>) -> Self` | binds the inner corpus (e.g. `"timeline:events/"`) |
| `ingest` | `async fn ingest(events: Vec<(String, serde_json::Map<String, serde_json::Value>)>) -> Result<(), LunarisError>` | forwards to `DocumentCorpus::ingest` (1 primitive call) |
| `between` | `async fn between(query: &str, lo: Hlc, hi: Hlc) -> Result<Vec<Hit>, LunarisError>` | events in **`[lo, hi)`** — lower inclusive, upper exclusive; 2 primitive calls (`TemporalQuery::<Documents>::new` + `.between(lo, hi).execute(query)`) |
| `as_of` | `async fn as_of(query: &str, ts: Hlc) -> Result<Vec<Hit>, LunarisError>` | the snapshot at `ts`; 2 primitive calls (`TemporalQuery::<Documents>::new` + `.as_of(ts).execute(query)`) |

### The boundary gotcha

`between` is **lower-bound inclusive, upper-bound exclusive** — the Phase
9.1 backend renderers emit `valid_from >= lo AND valid_from < hi` (Postgres)
and `@valid_time:[lo hi]` (Moon)
(`crates/lunaris-recipes/src/documentary/timeline_reconstruction.rs:15-19`).
To include "days 10 through 15 inclusive" (six days), pass `hi = Jan 16
00:00:00Z`, not `hi = Jan 15`. This carries straight into the Python / TS
parity tests too — same convention everywhere.

`Hlc`'s native shape is Unix-milliseconds; build bounds with
`Hlc::from_parts(unix_ms as u64, 0, 0)`.

## Example

Shaped after `timeline_reconstruction_between_returns_exactly_6_events` in
`crates/lunaris-recipes/tests/documentary_rust_integration.rs`:

```rust,no_run
use std::sync::Arc;
use lunaris::Lunaris;
use lunaris_core::hlc::Hlc;
use lunaris_recipes::documentary::TimelineReconstruction;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("moon://localhost:6379").await?);
    let timeline = TimelineReconstruction::new(lunaris.clone(), "timeline:events/");

    // Ingest dated events. Stamp the event's valid time into metadata so
    // your own queries can filter on it; the bi-temporal `valid_from` the
    // backend stamps drives `.between` / `.as_of`.
    let events = vec![
        (
            "Deploy 0.2.3 shipped to 5% of traffic.".to_string(),
            serde_json::Map::from_iter([
                ("event_id".to_string(), serde_json::json!("e-001")),
                ("event_valid_time_unix_ms".to_string(), serde_json::json!(1_736_467_200_000_i64)), // 2025-01-10
            ]),
        ),
        (
            "Deploy 0.2.3 rolled back after error spike.".to_string(),
            serde_json::Map::from_iter([
                ("event_id".to_string(), serde_json::json!("e-002")),
                ("event_valid_time_unix_ms".to_string(), serde_json::json!(1_736_726_400_000_i64)), // 2025-01-13
            ]),
        ),
    ];
    timeline.ingest(events).await?;

    // "What happened between Jan 10 and Jan 16?" — note hi is Jan 16, so
    // Jan 10..=Jan 15 are all included.
    let lo = Hlc::from_parts(1_736_467_200_000, 0, 0); // 2025-01-10T00:00:00Z
    let hi = Hlc::from_parts(1_737_072_000_000, 0, 0); // 2025-01-16T00:00:00Z (exclusive)
    let window = timeline.between("deploy 0.2.3", lo, hi).await?;
    println!("events in [Jan 10, Jan 16): {}", window.len());

    // "What did the timeline look like as of Jan 13?"
    let as_of_jan13 = Hlc::from_parts(1_736_726_400_000, 0, 0);
    let snapshot = timeline.as_of("deploy 0.2.3", as_of_jan13).await?;
    println!("snapshot hits: {}", snapshot.len());

    Ok(())
}
```

Swap the URL for `postgres://lunaris@localhost/lunaris` — the parity tests
assert Moon and Postgres return the *same* set for both `between` and
`as_of`.

## Notes

- **Always add a day to `hi`** if you mean an inclusive upper bound. This
  is the single doc-worthy footgun of this recipe.
- **`between` panics if `lo > hi`** — the bound check lives in
  `TemporalQuery::between` (`check_between_bounds`). Equal endpoints are
  allowed (empty-or-single-instant window).
- **No metadata filter on the recipe itself** — `TemporalQuery<Documents>`
  recalls across all Documents on the handle. Isolate distinct timelines
  with distinct prefixes (and a metadata filter via the underlying
  `DocumentCorpus`) or separate handles.
- For point-in-time recall over *code* rather than generic events, see
  [`CodeRepoMemory`](./research-and-code.md). For the bi-temporal MVCC model
  underneath, see [Durability & Recovery](../operations/durability.md).
