//! Plan 02-03: degraded_fallback operator integration tests.
//!
//! Validates that `degraded_fallback(primary, fallback)`:
//! 1. Returns primary's hits unchanged when primary succeeds (fallback NEVER called).
//! 2. Flips to fallback on primary error, tagging every returned hit with
//!    `degraded == true`.
//! 3. Composes end-to-end with the DSL: `Vector::new("chunks", 10)
//!    .degraded_fallback(Vector::new("entities", 10)).top(5)` — primary fails,
//!    fallback returns hits, hits land at the caller with `Hit { degraded: true }`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Hlc, HlcClock, LunarisError, StorageCapabilities, StorageError,
    StoragePort, StubEmbedder,
};
use lunaris_retrieve::{
    DegradedFallbackRetriever, Query, QueryContext, RawHit, RetrievalBuilder, Retriever, SourceOp,
    Vector,
};
use parking_lot::Mutex;
use serde_json::json;

// ============================================================ Fixtures

#[derive(Default)]
struct RecordingStorage {
    /// Per-index canned vector hits. `None` keyspaces return empty.
    vector_hits_by_index: Mutex<HashMap<String, Vec<VectorHit>>>,
    /// When set, vector_search PANICS for the matching index (used to
    /// simulate a hard backend failure for the primary path).
    panic_on_index: Mutex<Option<String>>,
    /// When set, vector_search returns an Err(StorageError::Backend(..)) for
    /// the matching index (preferred over panic for end-to-end fallback test).
    err_on_index: Mutex<Option<String>>,
    /// Counts vector_search calls per index — lets the "fallback NEVER called"
    /// test assert that count.
    calls_by_index: Mutex<HashMap<String, usize>>,
    chunks_by_key: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
}

impl RecordingStorage {
    fn new() -> Self {
        Self::default()
    }
    fn set_vector_hits(&self, index: &str, hits: Vec<VectorHit>) {
        self.vector_hits_by_index.lock().insert(index.to_string(), hits);
    }
    fn err_on(&self, index: &str) {
        *self.err_on_index.lock() = Some(index.to_string());
    }
    fn calls(&self, index: &str) -> usize {
        self.calls_by_index.lock().get(index).copied().unwrap_or(0)
    }
}

#[async_trait]
impl StoragePort for RecordingStorage {
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
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        *self.calls_by_index.lock().entry(index.to_string()).or_insert(0) += 1;
        if let Some(p) = self.panic_on_index.lock().clone()
            && p == index
        {
            panic!("RecordingStorage: simulated panic for index {p}");
        }
        if let Some(e) = self.err_on_index.lock().clone()
            && e == index
        {
            return Err(StorageError::Backend(format!("simulated backend drop for {e}")));
        }
        Ok(self.vector_hits_by_index.lock().get(index).cloned().unwrap_or_default())
    }
    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("graph_traverse"))
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
        if let Some(v) = self.chunks_by_key.lock().get(key).cloned() {
            return Ok(Some(Row {
                key: key.to_vec(),
                value: Bytes::from(v),
                bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
            }));
        }
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
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 768,
            native_rrf: false,
            max_scopes_recommended: 0,
        }
    }
}

#[async_trait]
impl KeywordPort for RecordingStorage {
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

fn vh(id: &[u8], score: f32) -> VectorHit {
    VectorHit { id: id.to_vec(), score, rerank_applied: false, metadata: json!({}) }
}

fn build_ctx(
    rec: Arc<RecordingStorage>,
) -> (Arc<dyn StoragePort>, Arc<dyn KeywordPort>, Arc<dyn Embedder>) {
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    (storage, keyword, embedder)
}

fn seed_chunk(rec: &RecordingStorage, text: &str) -> Vec<u8> {
    use lunaris_core::keyspace::chunk_key;
    use lunaris_core::primitives::Chunk;
    let clock = HlcClock::new(0);
    let episode_id = ulid::Ulid::new();
    let scope = lunaris_core::Scope::dev();
    let chunk = Chunk::new(scope.clone(), episode_id, text, 4, 0, vec![], &clock);
    let id_bytes = chunk.id.to_bytes().to_vec();
    // v0.2.1 keyspace (RFC 0001): `lunaris:{scope}:chunk:{ulid}`. Use the
    // canonical helper so this fixture tracks the keyspace format.
    let key = chunk_key(&scope, chunk.id);
    rec.chunks_by_key.lock().insert(key, serde_json::to_vec(&chunk).unwrap());
    id_bytes
}

// ============================================================ Recording Retrievers

/// Stub retriever that records call count and returns canned hits / err.
struct CountingRetriever {
    calls: Arc<AtomicUsize>,
    response: Result<Vec<RawHit>, String>,
}
impl CountingRetriever {
    fn ok(hits: Vec<RawHit>) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (Self { calls: calls.clone(), response: Ok(hits) }, calls)
    }
    fn err(msg: &str) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (Self { calls: calls.clone(), response: Err(msg.to_string()) }, calls)
    }
}
#[async_trait]
impl Retriever for CountingRetriever {
    async fn retrieve(&self, _ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.response {
            Ok(hits) => Ok(hits.clone()),
            Err(msg) => Err(LunarisError::Storage(StorageError::Backend(msg.clone()))),
        }
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn rh(id: &[u8], score: f32) -> RawHit {
    RawHit {
        id: id.to_vec(),
        score,
        rerank_applied: false,
        degraded: false,
        metadata: json!({}),
        source_op: SourceOp::Vector,
    }
}

// ============================================================ Tests

#[tokio::test]
async fn primary_ok_fallback_unused() {
    // Primary succeeds. Fallback MUST NOT be called.
    let (primary, primary_calls) = CountingRetriever::ok(vec![rh(b"a", 0.9), rh(b"b", 0.5)]);
    let (fallback, fallback_calls) = CountingRetriever::ok(vec![rh(b"z", 0.1)]);
    let r = DegradedFallbackRetriever::new(Box::new(primary), Box::new(fallback));

    let rec = Arc::new(RecordingStorage::new());
    let (storage, keyword, embedder) = build_ctx(rec);
    let ctx =
        QueryContext::new(Query::text("q"), lunaris_core::Scope::dev(), embedder, storage, keyword);

    let raw = r.retrieve(&ctx).await.unwrap();

    assert_eq!(primary_calls.load(Ordering::SeqCst), 1, "primary called once");
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 0, "fallback MUST NOT be called");
    assert_eq!(raw.len(), 2);
    // Primary's hits flow through with degraded == false.
    for h in &raw {
        assert!(!h.degraded, "primary success path MUST NOT set degraded=true");
    }
}

#[tokio::test]
async fn primary_err_flips_to_fallback_with_degraded_flag() {
    let (primary, primary_calls) = CountingRetriever::err("simulated drop");
    let (fallback, fallback_calls) = CountingRetriever::ok(vec![rh(b"x", 0.7), rh(b"y", 0.3)]);
    let r = DegradedFallbackRetriever::new(Box::new(primary), Box::new(fallback));

    let rec = Arc::new(RecordingStorage::new());
    let (storage, keyword, embedder) = build_ctx(rec);
    let ctx =
        QueryContext::new(Query::text("q"), lunaris_core::Scope::dev(), embedder, storage, keyword);

    let raw = r.retrieve(&ctx).await.unwrap();

    assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1, "fallback MUST run on primary err");
    assert_eq!(raw.len(), 2);
    for h in &raw {
        assert!(h.degraded, "fallback hits MUST be tagged degraded=true");
    }
}

#[tokio::test]
async fn end_to_end_with_recording_storage() {
    // Primary index "chunks" — RecordingStorage will return Err for this index.
    // Fallback index "entities" — succeeds with seeded hits.
    let rec = Arc::new(RecordingStorage::new());
    let id_e1 = seed_chunk(&rec, "entity one body");
    let id_e2 = seed_chunk(&rec, "entity two body");
    rec.set_vector_hits("entities", vec![vh(&id_e1, 0.8), vh(&id_e2, 0.6)]);
    rec.err_on("chunks"); // primary errors on this index

    let (storage, keyword, embedder) = build_ctx(rec.clone());

    // Build the canonical RETRIEVE-08 example: primary on chunks, fallback on entities.
    let root = Vector::new("chunks", 10).degraded_fallback(Vector::new("entities", 10)).top(5);
    let builder = RetrievalBuilder::new(storage, keyword, embedder).with_root(root);

    let hits = builder.execute(Query::text("query")).await.unwrap();

    // Both fallback hits hydrate (chunk rows are present). Each MUST carry
    // degraded=true so the caller can render "stale result" UI.
    assert_eq!(hits.len(), 2, "fallback must return both seeded entity hits");
    for h in &hits {
        assert!(h.degraded, "end-to-end Hit MUST report degraded=true");
    }

    // Primary AND fallback both got called via recording storage.
    assert_eq!(rec.calls("chunks"), 1, "primary called once before erroring");
    assert_eq!(rec.calls("entities"), 1, "fallback called once after primary err");
}

#[tokio::test]
async fn fallback_error_propagates_to_caller() {
    // Both primary AND fallback fail — the operator MUST surface the
    // fallback's error (the recall is genuinely unavailable).
    let (primary, _) = CountingRetriever::err("primary down");
    let (fallback, _) = CountingRetriever::err("fallback also down");
    let r = DegradedFallbackRetriever::new(Box::new(primary), Box::new(fallback));

    let rec = Arc::new(RecordingStorage::new());
    let (storage, keyword, embedder) = build_ctx(rec);
    let ctx =
        QueryContext::new(Query::text("q"), lunaris_core::Scope::dev(), embedder, storage, keyword);

    let res = r.retrieve(&ctx).await;
    let err = res.expect_err("both backends down MUST surface error");
    let msg = format!("{err}");
    assert!(
        msg.contains("fallback also down"),
        "operator MUST surface fallback's error when both fail; got: {msg}"
    );
}

#[tokio::test]
async fn nested_degraded_fallback_still_tags_terminal_hits() {
    // primary -> fallback1 (also fails) -> fallback2 (succeeds)
    let (primary, _) = CountingRetriever::err("p down");
    let (fallback1, _) = CountingRetriever::err("f1 down");
    let (fallback2, _) = CountingRetriever::ok(vec![rh(b"final", 0.9)]);

    let inner = DegradedFallbackRetriever::new(Box::new(fallback1), Box::new(fallback2));
    let outer = DegradedFallbackRetriever::new(Box::new(primary), Box::new(inner));

    let rec = Arc::new(RecordingStorage::new());
    let (storage, keyword, embedder) = build_ctx(rec);
    let ctx =
        QueryContext::new(Query::text("q"), lunaris_core::Scope::dev(), embedder, storage, keyword);

    let raw = outer.retrieve(&ctx).await.unwrap();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].id, b"final".to_vec());
    assert!(raw[0].degraded, "any fallback path MUST set degraded=true");
}
