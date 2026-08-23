//! RED — `TimelineReconstruction` recall ignores the `source_prefix` it was
//! constructed with (F30).
//!
//! `TimelineReconstruction::new(lunaris, scope, source_prefix)` hands the
//! prefix to a `DocumentCorpus` and keeps it as `self.corpus`. `ingest`
//! forwards there, so writes ARE prefixed. But `between` and `as_of` never
//! touch `self.corpus` — they build
//! `TemporalQuery::<Documents>::new(self.lunaris)`, which takes neither a
//! prefix nor a scope. The read path therefore returns every timeline event in
//! the store inside the window, whatever its source.
//!
//! The sibling recipe settles what the contract is: `DocumentCorpus::search`
//! post-filters `h.source.starts_with(&prefix)` after hydrate, with a comment
//! explaining why the filter cannot live in the `StoragePort` (Moon's `chunks`
//! FT schema has no `source` field). `TimelineReconstruction`'s temporal reads
//! skip that step.
//!
//! ## How this shipped unnoticed
//!
//! `conformance-bindings` went red on `main` on 2026-08-22 with
//! `timeline_reconstruction_parity_between_10_and_15`: `expected 12 to be 6`.
//! The `per-driver parity (moon)` job runs Rust, Python and TypeScript against
//! ONE Moon with no flush between them. Python ingests the 30-event fixture
//! under `timeline:doc-11-03-py/moon/`; TypeScript then ingests it under
//! `timeline:doc-11-03-ts/moon/`. Python runs first and sees its own 6;
//! TypeScript runs second and sees 6 + 6. Before the bi-temporal VALID axis
//! became real (`238db43`) the graph-OFF ingest path wrote no `valid_time_ms`,
//! so `.between` never selected the other language's rows and the bug was
//! invisible.
//!
//! Two timelines in one scope is the ordinary case the `source_prefix`
//! parameter exists to serve, so this is a product defect. Giving the two
//! suites unique prefixes, or flushing between steps, would have turned the
//! board green while leaving callers broken.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::Lunaris;
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Hlc, HlcClock, LunarisError, StorageCapabilities, StorageError,
    StoragePort,
};
use parking_lot::Mutex;

const DIM: usize = 768;

struct StubEmbedder;

#[async_trait]
impl Embedder for StubEmbedder {
    fn dim(&self) -> usize {
        DIM
    }
    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        Ok(inputs.iter().map(|_| vec![0.1f32; DIM]).collect())
    }
}

/// Storage that records every atomic_write batch (KvPut rows readable back).
#[derive(Default)]
struct OpRecordingStorage {
    rows: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    batches: Mutex<Vec<Vec<WriteOp>>>,
}

#[async_trait]
impl StoragePort for OpRecordingStorage {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        for op in ops {
            if let WriteOp::KvPut { key, value } = op {
                self.rows.lock().insert(key.clone(), value.clone());
            }
        }
        self.batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: 1, counter: 1 })
    }
    /// Deliberately PERMISSIVE: returns every chunk ever upserted into
    /// `index`, ignoring the query vector, `k`, and the filter.
    ///
    /// A backend that filtered would mask the defect under test. Moon does not
    /// filter this either — `source` is not a field on the `chunks` FT schema,
    /// which is exactly why `DocumentCorpus::search` post-filters on
    /// `Hit.source` after hydrate. The recipe is the only layer that can apply
    /// the source prefix, so the recipe is the layer this test pins.
    async fn vector_search(
        &self,
        _scope: &lunaris_core::Scope,
        index: &str,
        _query: &[f32],
        _k: usize,
        filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Ok(self
            .batches
            .lock()
            .iter()
            .flatten()
            .filter_map(|op| match op {
                WriteOp::VectorUpsert { index: i, id, metadata, .. } if i == index => {
                    // Honour ONLY the valid-time window — the one filter a real
                    // backend can push down here. `source` is deliberately not
                    // honoured: it is not a field on Moon's `chunks` FT schema,
                    // so the recipe is the only layer that can apply it.
                    if !in_window(metadata, filter) {
                        return None;
                    }
                    Some(VectorHit {
                        id: id.clone(),
                        score: 1.0,
                        rerank_applied: false,
                        metadata: metadata.clone(),
                    })
                }
                _ => None,
            })
            .collect())
    }
    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }
    async fn scan_range(
        &self,
        _scope: &lunaris_core::Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new()).boxed())
    }
    async fn read_as_of(
        &self,
        _scope: &lunaris_core::Scope,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.lock().get(key).cloned().map(|v| Row {
            key: key.to_vec(),
            value: Bytes::from(v),
            bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
        }))
    }
    async fn publish(
        &self,
        _s: &lunaris_core::Scope,
        _t: &str,
        _p: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }
    async fn subscribe(
        &self,
        _s: &lunaris_core::Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("OpRecordingStorage::subscribe"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: DIM as u32,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

#[async_trait]
impl KeywordPort for OpRecordingStorage {
    async fn keyword_search(
        &self,
        _scope: &lunaris_core::Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(Vec::new())
    }
}

/// Apply a `Filter::ValidTimeRange` to a chunk's `valid_time_ms`, lower
/// inclusive / upper exclusive — the boundary the recipe documents. Any other
/// filter, or none, matches everything.
fn in_window(metadata: &serde_json::Value, filter: Option<&Filter>) -> bool {
    let Some(Filter::ValidTimeRange { after, before }) = filter else {
        return true;
    };
    let Some(ms) = metadata.get("valid_time_ms").and_then(|v| v.as_i64()) else {
        return true;
    };
    if let Some(lo) = after
        && ms < lo.wall_ms as i64
    {
        return false;
    }
    if let Some(hi) = before
        && ms >= hi.wall_ms as i64
    {
        return false;
    }
    true
}

fn make_handle(rec: Arc<OpRecordingStorage>, clock: Arc<HlcClock>) -> Arc<Lunaris> {
    Arc::new(Lunaris::with_parts_keyword(
        rec.clone() as Arc<dyn StoragePort>,
        rec as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder) as Arc<dyn Embedder>,
        clock,
    ))
}

/// The event both timelines declare, so valid-time cannot be what separates
/// them. 2025-01-13T10:00:00Z — inside the `[10, 16)` window used below.
const VALID_MS: i64 = 1_736_762_400_000;

fn window() -> (Hlc, Hlc) {
    // 2025-01-10T00:00:00Z .. 2025-01-16T00:00:00Z, lower-inclusive /
    // upper-exclusive, matching the recipe's documented boundary.
    (
        Hlc { wall_ms: 1_736_467_200_000, counter: 0, node_id: 0 },
        Hlc { wall_ms: 1_736_985_600_000, counter: 0, node_id: 0 },
    )
}

fn meta(event_id: &str) -> serde_json::Map<String, serde_json::Value> {
    [
        ("event_id".to_string(), serde_json::json!(event_id)),
        (lunaris_recipes::DocumentCorpus::VALID_TIME_KEY.to_string(), serde_json::json!(VALID_MS)),
    ]
    .into_iter()
    .collect()
}

/// Two timelines in one scope, each bound to its own `source_prefix`, each
/// holding one event at the same instant. Recall on one must not see the other.
///
/// This is the CI failure in miniature: 1 expected, 2 returned is the same
/// defect as 6 expected, 12 returned.
#[tokio::test]
async fn a_timeline_recalls_only_its_own_prefix() {
    let rec = Arc::new(OpRecordingStorage::default());
    let handle = make_handle(rec.clone(), HlcClock::new(0));

    let alpha = lunaris_recipes::documentary::TimelineReconstruction::new(
        handle.clone(),
        lunaris_core::Scope::dev(),
        "timeline:alpha/",
    );
    let beta = lunaris_recipes::documentary::TimelineReconstruction::new(
        handle,
        lunaris_core::Scope::dev(),
        "timeline:beta/",
    );

    alpha
        .ingest(vec![("The alpha relay outage began in the morning.".to_string(), meta("a-1"))])
        .await
        .expect("alpha ingest must succeed");
    beta.ingest(vec![("The beta relay outage began in the morning.".to_string(), meta("b-1"))])
        .await
        .expect("beta ingest must succeed");

    let (lo, hi) = window();
    let hits = beta.between("outage", lo, hi).await.expect("between must succeed");

    let sources: Vec<&str> = hits.iter().map(|h| h.source.as_str()).collect();
    assert!(
        sources.iter().all(|s| s.starts_with("timeline:beta/")),
        "beta's timeline returned a row from another source prefix. \
         `between` builds a bare TemporalQuery and never applies the corpus's \
         source prefix, so every timeline in the scope answers every query. \
         sources: {sources:?}"
    );
    assert_eq!(
        hits.len(),
        1,
        "expected exactly beta's single event; got {}. sources: {sources:?}",
        hits.len()
    );
}

/// Vacuity floor. Without this, a hydrate path that returned NOTHING would
/// satisfy the `all(starts_with)` assertion above trivially — an empty
/// iterator satisfies `all`. This test fails if the fixture cannot produce
/// beta's own row, so the assertion above is only ever read as a real result.
#[tokio::test]
async fn the_fixture_can_return_the_timelines_own_event() {
    let rec = Arc::new(OpRecordingStorage::default());
    let handle = make_handle(rec.clone(), HlcClock::new(0));

    let beta = lunaris_recipes::documentary::TimelineReconstruction::new(
        handle,
        lunaris_core::Scope::dev(),
        "timeline:beta/",
    );
    beta.ingest(vec![("The beta relay outage began in the morning.".to_string(), meta("b-1"))])
        .await
        .expect("beta ingest must succeed");

    let (lo, hi) = window();
    let hits = beta.between("outage", lo, hi).await.expect("between must succeed");

    assert_eq!(
        hits.len(),
        1,
        "the fixture itself is broken: a timeline holding exactly one in-window \
         event returned {} rows. Every other assertion in this file is \
         meaningless until this passes.",
        hits.len()
    );
    assert!(
        hits[0].source.starts_with("timeline:beta/"),
        "hydrate did not populate Hit.source; the prefix assertions cannot \
         mean anything. got {:?}",
        hits[0].source
    );
}

/// The post-filter shrinks the result set, and `TemporalQuery` exposes no
/// `top_k` to over-fetch against. At CI's scale — two 30-event timelines in
/// one scope, six of each inside the window — a default `k` smaller than the
/// combined match set truncates BEFORE the prefix filter runs, and the caller
/// silently gets fewer than their own six.
///
/// This is the failure mode the fix could have traded for the old one, so it
/// is pinned at the size the real suites use rather than at the two rows the
/// tests above need.
#[tokio::test]
async fn a_crowded_window_still_returns_every_one_of_this_timelines_events() {
    let rec = Arc::new(OpRecordingStorage::default());
    let handle = make_handle(rec.clone(), HlcClock::new(0));

    let alpha = lunaris_recipes::documentary::TimelineReconstruction::new(
        handle.clone(),
        lunaris_core::Scope::dev(),
        "timeline:alpha/",
    );
    let beta = lunaris_recipes::documentary::TimelineReconstruction::new(
        handle,
        lunaris_core::Scope::dev(),
        "timeline:beta/",
    );

    // 30 events each, one per day from 2025-01-01, exactly as the parity
    // fixture does. Six of each land inside the [10, 16) window.
    const DAY_MS: i64 = 86_400_000;
    const JAN_1: i64 = 1_735_689_600_000; // 2025-01-01T00:00:00Z
    for day in 0..30i64 {
        let ms = JAN_1 + day * DAY_MS;
        let stamped = |who: &str| {
            let mut m = meta(&format!("{who}-{day}"));
            m.insert(
                lunaris_recipes::DocumentCorpus::VALID_TIME_KEY.to_string(),
                serde_json::json!(ms),
            );
            m
        };
        alpha
            .ingest(vec![(format!("alpha outage event on day {day}"), stamped("a"))])
            .await
            .expect("alpha ingest must succeed");
        beta.ingest(vec![(format!("beta outage event on day {day}"), stamped("b"))])
            .await
            .expect("beta ingest must succeed");
    }

    let (lo, hi) = window();
    let hits = beta.between("outage", lo, hi).await.expect("between must succeed");

    let foreign: Vec<&str> = hits
        .iter()
        .map(|h| h.source.as_str())
        .filter(|s| !s.starts_with("timeline:beta/"))
        .collect();
    assert!(foreign.is_empty(), "leaked another timeline's events: {foreign:?}");
    assert_eq!(
        hits.len(),
        6,
        "expected all six of beta's in-window events; got {}. Fewer than six \
         means the backend's default k truncated the combined match set before \
         the prefix post-filter ran — the recipe needs an over-fetch, not just \
         a filter.",
        hits.len()
    );
}
