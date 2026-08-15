//! ADD task `ft-navigate-recall` — Navigate operator capability gating +
//! Graph::anchored decay threading (contract FROZEN @ v1, 2026-06-11).
//! Mock-port suite; no live backend.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphDecay, GraphResult, Lsn, NavigateHit, NavigateSpec, QueueMsg, Row,
    VectorHit, WriteOp,
};
use lunaris_core::{Embedder, Hlc, StorageCapabilities, StorageError, StoragePort, StubEmbedder};
use lunaris_retrieve::{Graph, Navigate, Query, QueryContext, Retriever};
use parking_lot::Mutex;
use serde_json::json;

// ============================================================ Fixture

#[derive(Default)]
struct NavRecordingStorage {
    navigate_native: bool,
    vector_hits: Mutex<Vec<VectorHit>>,
    navigate_hits: Mutex<Vec<NavigateHit>>,
    vector_calls: Arc<AtomicUsize>,
    navigate_calls: Arc<AtomicUsize>,
    /// ft-navigate-filter-gap contract v1 — records the Debug rendering of
    /// the filter passed to each vector_search call (None = unfiltered call).
    vector_filters: Mutex<Vec<Option<String>>>,
    /// KG-RAG facts-as-graph-nodes — records the INDEX each vector_search
    /// fallback targets. The degraded path's index is a correctness question,
    /// not a detail: a facts leg that KNN-seeds `entities` for graph hops must
    /// fall back to `facts`, or backends without native navigate lose the
    /// fact vector leg entirely.
    vector_indexes: Mutex<Vec<String>>,
    /// Records the decay lambda passed to graph_traverse_decayed (None when
    /// called without decay; outer None = never called).
    decayed_calls: Mutex<Vec<Option<f64>>>,
}

impl NavRecordingStorage {
    fn with_native(native: bool) -> Self {
        Self { navigate_native: native, ..Default::default() }
    }
}

#[async_trait]
impl StoragePort for NavRecordingStorage {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        _ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
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
        self.vector_calls.fetch_add(1, Ordering::SeqCst);
        self.vector_filters.lock().push(filter.map(|f| format!("{f:?}")));
        self.vector_indexes.lock().push(index.to_owned());
        Ok(self.vector_hits.lock().clone())
    }
    async fn vector_navigate(
        &self,
        _scope: &lunaris_core::Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _spec: &NavigateSpec,
    ) -> Result<Vec<NavigateHit>, StorageError> {
        self.navigate_calls.fetch_add(1, Ordering::SeqCst);
        if !self.navigate_native {
            return Err(StorageError::NotSupported(
                "graph_navigate_unsupported: backend has no native navigate",
            ));
        }
        Ok(self.navigate_hits.lock().clone())
    }
    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult { headers: vec![], rows: vec![] })
    }
    async fn graph_traverse_decayed(
        &self,
        scope: &lunaris_core::Scope,
        q: &CypherQuery,
        as_of: Option<Hlc>,
        decay: Option<&GraphDecay>,
    ) -> Result<GraphResult, StorageError> {
        self.decayed_calls.lock().push(decay.map(|d| d.lambda()));
        self.graph_traverse(scope, q, as_of).await
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
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(None)
    }
    async fn publish(
        &self,
        _scope: &lunaris_core::Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("publish"))
    }
    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("subscribe"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: true,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 768,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: self.navigate_native,
            graph_navigate_native: self.navigate_native,
        }
    }
}

#[async_trait]
impl KeywordPort for NavRecordingStorage {
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

fn ctx_for(rec: Arc<NavRecordingStorage>) -> QueryContext {
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec;
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    QueryContext::new(Query::text("q"), lunaris_core::Scope::dev(), embedder, storage, keyword)
}

fn vh(id: &[u8], score: f32) -> VectorHit {
    VectorHit { id: id.to_vec(), score, rerank_applied: false, metadata: json!({}) }
}

fn nh(id: &[u8], vec_score: f32, hop_depth: u32, final_score: f32) -> NavigateHit {
    NavigateHit { id: id.to_vec(), vec_score, hop_depth, final_score }
}

// ============================================================ Tests

/// §2 capability gating — graph_navigate_native=true routes through the
/// port's vector_navigate and graph-expanded hits carry hop provenance.
///
/// Re-baselined 2 → 1 hit alongside the hop-0 seed drop (KG-RAG per-leg RRF
/// wave): the KNN seeds a Navigate hop starts from are entity nodes with no
/// KV row, so hydration discards them anyway — but only AFTER `.top(k)`, so
/// every seed that reached fusion burned a reader-context slot and then
/// vanished. The leg's payload is the graph-EXPANDED content (hop >= 1).
/// The drop itself is pinned end-to-end by
/// `per_leg_rrf.rs::navigate_hop0_entity_seeds_never_reach_fusion`.
#[tokio::test]
async fn navigate_uses_port_when_native() {
    let rec = Arc::new(NavRecordingStorage::with_native(true));
    *rec.navigate_hits.lock() = vec![nh(b"seed", 0.01, 0, 0.01), nh(b"expanded", 0.0, 1, 0.1)];
    let ctx = ctx_for(rec.clone());

    let op = Navigate::new("entities", 5).with_hops(2).expect("valid hops");
    let hits = op.retrieve(&ctx).await.expect("navigate retrieve");

    assert_eq!(rec.navigate_calls.load(Ordering::SeqCst), 1, "port navigate called once");
    assert_eq!(rec.vector_calls.load(Ordering::SeqCst), 0, "no vector fallback on native path");
    assert_eq!(hits.len(), 1, "the hop-0 KNN seed is dropped; only graph-expanded hits survive");
    assert!(hits.iter().all(|h| h.id != b"seed".to_vec()), "hop-0 seed must not reach the caller");
    let expanded = hits.iter().find(|h| h.id == b"expanded".to_vec()).expect("expanded hit");
    assert_eq!(
        expanded.metadata.get("hop_depth").and_then(|v| v.as_u64()),
        Some(1),
        "graph-expanded hit must carry hop_depth provenance; got {:?}",
        expanded.metadata
    );
}

/// §2 degradation — capability false: Navigate returns plain vector hits,
/// never errors, never calls the navigate port method.
#[tokio::test]
async fn navigate_degrades_to_vector_on_unsupported() {
    let rec = Arc::new(NavRecordingStorage::with_native(false));
    *rec.vector_hits.lock() = vec![vh(b"v1", 0.9), vh(b"v2", 0.5)];
    let ctx = ctx_for(rec.clone());

    let op = Navigate::new("entities", 5).with_hops(2).expect("valid hops");
    let hits = op.retrieve(&ctx).await.expect("degraded navigate must not error");

    assert_eq!(rec.navigate_calls.load(Ordering::SeqCst), 0, "capability=false short-circuits");
    assert_eq!(rec.vector_calls.load(Ordering::SeqCst), 1, "vector fallback called once");
    let ids: Vec<_> = hits.iter().map(|h| h.id.clone()).collect();
    assert_eq!(ids, vec![b"v1".to_vec(), b"v2".to_vec()], "vector hits flow through unchanged");
}

/// KG-RAG facts-as-graph-nodes — the fused root's facts leg is
/// `Navigate::new("entities", k)`: it KNN-seeds on ENTITY similarity and
/// reaches facts by graph hop. On a backend with no native navigate there is
/// no hop, so the degraded vector search must target the leg's CONTENT index
/// (`facts`) — searching the seed index would return entity docs and the
/// fact vector leg would silently vanish. Moon is the only shipped backend
/// with native navigate today, but the degraded path is still live on it:
/// any FILTERED query has no native navigate surface and falls through here.
#[tokio::test]
async fn navigate_degraded_fallback_targets_fallback_index_when_set() {
    let rec = Arc::new(NavRecordingStorage::with_native(false));
    *rec.vector_hits.lock() = vec![vh(b"f1", 0.8)];
    let ctx = ctx_for(rec.clone());

    let op = Navigate::new("entities", 5).with_fallback_index("facts");
    let hits = op.retrieve(&ctx).await.expect("degraded navigate must not error");

    assert_eq!(
        rec.vector_indexes.lock().clone(),
        vec!["facts".to_string()],
        "the degraded facts leg must vector-search `facts`, not the `entities` KNN seed index"
    );
    assert_eq!(
        hits[0].metadata.get("index").and_then(|v| v.as_str()),
        Some("facts"),
        "hits must be tagged with the index they actually came from (per-leg RRF bucketing \
         and the chunk-floor selector both read this tag); got {:?}",
        hits[0].metadata
    );
}

/// Regression pin — with no fallback configured the degraded path keeps
/// searching the seed index, exactly as before the fallback existed.
#[tokio::test]
async fn navigate_degraded_fallback_defaults_to_seed_index() {
    let rec = Arc::new(NavRecordingStorage::with_native(false));
    *rec.vector_hits.lock() = vec![vh(b"e1", 0.8)];
    let ctx = ctx_for(rec.clone());

    let op = Navigate::new("entities", 5);
    let _ = op.retrieve(&ctx).await.expect("degraded navigate must not error");

    assert_eq!(
        rec.vector_indexes.lock().clone(),
        vec!["entities".to_string()],
        "no fallback index configured => seed index unchanged"
    );
}

/// The filter-gap degradation (native backend, filtered query) takes the same
/// vector path and must honour the fallback index too — otherwise every
/// FILTERED recall loses the fact vector leg even on Moon.
#[tokio::test]
async fn filtered_navigate_on_native_backend_uses_fallback_index() {
    let rec = Arc::new(NavRecordingStorage::with_native(true));
    *rec.vector_hits.lock() = vec![vh(b"f1", 0.8)];
    let mut ctx = ctx_for(rec.clone());
    ctx.query.filter = Some(Filter::Eq { field: "source".into(), value: json!("alpha.md") });

    let op = Navigate::new("entities", 5).with_fallback_index("facts");
    let _ = op.retrieve(&ctx).await.expect("filtered navigate retrieves");

    assert_eq!(rec.navigate_calls.load(Ordering::SeqCst), 0, "filter present => no native path");
    assert_eq!(
        rec.vector_indexes.lock().clone(),
        vec!["facts".to_string()],
        "filtered degradation must also search the leg's content index"
    );
}

/// §1 Reject — invalid hops / hop_penalty fail at construction, before IO.
#[tokio::test]
async fn navigate_invalid_params_fail_construction() {
    let err = Navigate::new("entities", 5).with_hops(0).expect_err("hops 0 rejected");
    assert!(format!("{err}").contains("navigate_invalid_hops"), "named code, got {err}");
    let err = Navigate::new("entities", 5).with_hops(6).expect_err("hops 6 rejected");
    assert!(format!("{err}").contains("navigate_invalid_hops"), "named code, got {err}");

    let err = Navigate::new("entities", 5)
        .with_hops(2)
        .expect("valid hops")
        .with_hop_penalty(f64::NAN)
        .expect_err("NaN penalty rejected");
    assert!(format!("{err}").contains("navigate_invalid_hop_penalty"), "named code, got {err}");
}

/// ft-navigate-filter-gap contract v1 §2 scenario 1 — a filtered Navigate on
/// a native backend must route to the filter-enforcing vector path: the guard
/// sits ABOVE the capability gate, so vector_search receives the exact filter
/// and vector_navigate is never called (reject: silent-ignore).
#[tokio::test]
async fn filtered_navigate_routes_to_filtered_vector_search() {
    let rec = Arc::new(NavRecordingStorage::with_native(true));
    *rec.vector_hits.lock() = vec![vh(b"a1", 0.9)];
    // Would-be navigate hits: if the (buggy) native path ran, these surface.
    *rec.navigate_hits.lock() = vec![nh(b"a1", 0.01, 0, 0.01), nh(b"foreign", 0.0, 1, 0.1)];
    let mut ctx = ctx_for(rec.clone());
    ctx.query.filter = Some(Filter::Eq { field: "source".into(), value: json!("alpha.md") });

    let op = Navigate::new("entities", 5).with_hops(2).expect("valid hops");
    let hits = op.retrieve(&ctx).await.expect("filtered navigate retrieves");

    assert_eq!(
        rec.navigate_calls.load(Ordering::SeqCst),
        0,
        "filter present -> vector_navigate must NOT run (it has no filter surface)"
    );
    assert_eq!(rec.vector_calls.load(Ordering::SeqCst), 1, "filtered vector fallback called once");
    let filters = rec.vector_filters.lock().clone();
    assert_eq!(filters.len(), 1);
    let recorded = filters[0].as_deref().expect("vector_search must receive Some(filter)");
    assert!(recorded.contains("alpha.md"), "the exact filter must reach the port; got {recorded}");
    let ids: Vec<_> = hits.iter().map(|h| h.id.clone()).collect();
    assert_eq!(ids, vec![b"a1".to_vec()], "hits come from the filtered vector path");
}

/// ft-navigate-filter-gap contract v1 §2 scenario 4 (regression pin) — the
/// pre-existing capability-gated degradation keeps passing the filter through.
#[tokio::test]
async fn filtered_navigate_nonnative_keeps_filter() {
    let rec = Arc::new(NavRecordingStorage::with_native(false));
    *rec.vector_hits.lock() = vec![vh(b"v1", 0.9)];
    let mut ctx = ctx_for(rec.clone());
    ctx.query.filter = Some(Filter::Eq { field: "source".into(), value: json!("alpha.md") });

    let op = Navigate::new("entities", 5).with_hops(2).expect("valid hops");
    let _ = op.retrieve(&ctx).await.expect("non-native filtered navigate retrieves");

    assert_eq!(rec.navigate_calls.load(Ordering::SeqCst), 0, "capability=false short-circuits");
    let filters = rec.vector_filters.lock().clone();
    assert_eq!(filters.len(), 1);
    assert!(
        filters[0].as_deref().is_some_and(|f| f.contains("alpha.md")),
        "filter must reach vector_search on the non-native path; got {filters:?}"
    );
}

/// §2 decay surface — Graph::anchored(...).with_decay(d) threads the decay
/// through StoragePort::graph_traverse_decayed.
#[tokio::test]
async fn anchored_with_decay_threads_through() {
    let rec = Arc::new(NavRecordingStorage::with_native(true));
    let ctx = ctx_for(rec.clone());

    let seeds = vec![(lunaris_extract::EntityId([7u8; 16]), 0.9)];
    let decay = GraphDecay::new(3.0).expect("valid λ");
    let op = Graph::anchored(seeds, 2).with_decay(decay);
    let _ = op.retrieve(&ctx).await.expect("anchored with decay retrieves");

    let calls = rec.decayed_calls.lock().clone();
    assert!(
        calls.contains(&Some(3.0)),
        "graph_traverse_decayed must be called with λ=3.0; recorded: {calls:?}"
    );
}

/// §2 decay surface — anchored WITHOUT decay keeps today's behavior (no
/// decay lambda reaches the port).
#[tokio::test]
async fn anchored_without_decay_unchanged() {
    let rec = Arc::new(NavRecordingStorage::with_native(true));
    let ctx = ctx_for(rec.clone());

    let seeds = vec![(lunaris_extract::EntityId([7u8; 16]), 0.9)];
    let op = Graph::anchored(seeds, 2);
    let _ = op.retrieve(&ctx).await.expect("anchored retrieves");

    let calls = rec.decayed_calls.lock().clone();
    assert!(
        !calls.iter().any(|c| c.is_some()),
        "no decay must reach the port without with_decay; recorded: {calls:?}"
    );
}
