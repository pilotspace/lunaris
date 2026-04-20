//! Plan 02-03: rerank operator composition tests.
//!
//! Validates that the `rerank(model)` operator:
//! 1. Preserves upstream order when wired with `NoopReranker`.
//! 2. Re-orders hits when wired with a re-sorting reranker (proves the rerank
//!    pass actually flows through the pipeline).
//! 3. Truncates upstream candidates to `k_in` (default 30) before invoking
//!    the cross-encoder pass — defends the blueprint §4.2 12 ms budget by
//!    capping the rerank batch size.
//! 4. Partial-hydrates chunk text from storage so the cross-encoder can
//!    pair-encode `(query, doc.text)`.
//! 5. Sets `rerank_applied = reranker.applies()` on every output hit so
//!    callers can render "real cross-encoder ran" UI vs "fell back to noop".
//!
//! All tests run against an in-memory `RecordingStorage` — no Moon / Postgres
//! required.

use std::collections::HashMap;
use std::sync::Arc;

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
use lunaris_rerank::{NoopReranker, RerankCandidate, Reranker};
use lunaris_retrieve::{QueryContext, Query, RawHit, Retriever, SourceOp, Vector};
use parking_lot::Mutex;
use serde_json::json;

// ============================================================ Fixtures

#[derive(Default)]
struct RecordingStorage {
    vector_hits: Mutex<Vec<VectorHit>>,
    chunks_by_key: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
}

impl RecordingStorage {
    fn new() -> Self {
        Self::default()
    }
    fn set_vector_hits(&self, hits: Vec<VectorHit>) {
        *self.vector_hits.lock() = hits;
    }
}

#[async_trait]
impl StoragePort for RecordingStorage {
    async fn atomic_write(&self, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
    async fn vector_search(
        &self,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Ok(self.vector_hits.lock().clone())
    }
    async fn graph_traverse(
        &self,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::graph_traverse"))
    }
    async fn scan_range(
        &self,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new()).boxed())
    }
    async fn read_as_of(
        &self,
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
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::publish"))
    }
    async fn subscribe(
        &self,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::subscribe"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 768,
            native_rrf: false,
        }
    }
}

#[async_trait]
impl KeywordPort for RecordingStorage {
    async fn keyword_search(
        &self,
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

/// Inserts a real `lunaris_core::Chunk` into the recording storage so partial
/// hydration finds the chunk text. Returns the chunk's id bytes (so the
/// caller can build a matching VectorHit id).
fn seed_chunk(rec: &RecordingStorage, text: &str) -> Vec<u8> {
    use lunaris_core::primitives::Chunk;
    let clock = HlcClock::new(0);
    // Use a stable episode_id per chunk; tests don't depend on Episode lookup.
    let episode_id = ulid::Ulid::new();
    let chunk = Chunk::new(episode_id, text, 4, 0, vec!["Notes".to_string()], &clock);
    let id_bytes = chunk.id.to_bytes().to_vec();
    let key = format!("lunaris:chunk:{}", chunk.id).into_bytes();
    rec.chunks_by_key.lock().insert(key, serde_json::to_vec(&chunk).unwrap());
    id_bytes
}

// ============================================================ Mock rerankers

/// MockReranker that re-sorts docs by their id bytes (lexicographic descending).
/// Used to PROVE the rerank pass actually re-orders the upstream hits.
struct LexicographicReranker;

#[async_trait]
impl Reranker for LexicographicReranker {
    async fn rerank(
        &self,
        _query: &str,
        mut docs: Vec<RerankCandidate>,
    ) -> Result<Vec<RerankCandidate>, LunarisError> {
        // Sort by id descending so we can assert the operator reordered.
        docs.sort_by(|a, b| b.id.cmp(&a.id));
        // Replace scores with rank-derived values so downstream ordering is stable.
        let n = docs.len();
        for (i, d) in docs.iter_mut().enumerate() {
            d.score = (n - i) as f32;
        }
        Ok(docs)
    }
    fn applies(&self) -> bool {
        true
    }
}

/// RecordingReranker captures the candidate list it received.
struct RecordingReranker {
    received: Mutex<Vec<RerankCandidate>>,
}
impl RecordingReranker {
    fn new() -> Self {
        Self { received: Mutex::new(Vec::new()) }
    }
}
#[async_trait]
impl Reranker for RecordingReranker {
    async fn rerank(
        &self,
        _query: &str,
        docs: Vec<RerankCandidate>,
    ) -> Result<Vec<RerankCandidate>, LunarisError> {
        *self.received.lock() = docs.clone();
        Ok(docs)
    }
    fn applies(&self) -> bool {
        true
    }
}

// ============================================================ Tests

#[tokio::test]
async fn rerank_with_noop_preserves_order() {
    let rec = Arc::new(RecordingStorage::new());
    // Three vector hits in a fixed order; NoopReranker MUST preserve that order.
    let id_a = seed_chunk(&rec, "alpha document text");
    let id_b = seed_chunk(&rec, "beta document text");
    let id_c = seed_chunk(&rec, "gamma document text");
    rec.set_vector_hits(vec![vh(&id_a, 0.9), vh(&id_b, 0.7), vh(&id_c, 0.5)]);

    let (storage, keyword, embedder) = build_ctx(rec.clone());
    let ctx = QueryContext::new(Query::text("anything"), embedder, storage, keyword);

    let root = Vector::new("chunks", 30).rerank(Arc::new(NoopReranker));
    let raw = root.retrieve(&ctx).await.unwrap();

    assert_eq!(raw.len(), 3);
    // NoopReranker MUST preserve the upstream order (by score: a > b > c).
    assert_eq!(raw[0].id, id_a);
    assert_eq!(raw[1].id, id_b);
    assert_eq!(raw[2].id, id_c);
    // Every output hit MUST report rerank_applied == false (NoopReranker.applies()=false).
    for h in &raw {
        assert!(!h.rerank_applied, "NoopReranker MUST set rerank_applied=false");
        assert_eq!(h.source_op, SourceOp::Reranked);
    }
}

#[tokio::test]
async fn rerank_with_mock_inverts_order() {
    let rec = Arc::new(RecordingStorage::new());
    // Use deterministic id bytes by using the seed ordering — but we want
    // the LexicographicReranker to flip the order, so use 3 chunks and
    // assert that the OUTPUT is sorted by id descending (regardless of
    // their upstream score).
    let id_a = seed_chunk(&rec, "doc a");
    let id_b = seed_chunk(&rec, "doc b");
    let id_c = seed_chunk(&rec, "doc c");
    rec.set_vector_hits(vec![vh(&id_a, 0.9), vh(&id_b, 0.7), vh(&id_c, 0.5)]);

    let (storage, keyword, embedder) = build_ctx(rec.clone());
    let ctx = QueryContext::new(Query::text("q"), embedder, storage, keyword);

    let root = Vector::new("chunks", 30).rerank(Arc::new(LexicographicReranker));
    let raw = root.retrieve(&ctx).await.unwrap();

    assert_eq!(raw.len(), 3);
    // Compute expected order: id sorted descending.
    let mut expected = vec![id_a.clone(), id_b.clone(), id_c.clone()];
    expected.sort_by(|a, b| b.cmp(a));
    let actual: Vec<Vec<u8>> = raw.iter().map(|h| h.id.clone()).collect();
    assert_eq!(actual, expected, "LexicographicReranker MUST sort hits by id desc");
    for h in &raw {
        assert!(h.rerank_applied, "real reranker MUST set rerank_applied=true");
        assert_eq!(h.source_op, SourceOp::Reranked);
    }
}

#[tokio::test]
async fn rerank_truncates_to_k_in() {
    let rec = Arc::new(RecordingStorage::new());
    // Seed 50 vector hits — the rerank operator MUST truncate to k_in (30 default)
    // BEFORE calling the reranker, defending the blueprint §4.2 latency budget.
    let mut hits = Vec::with_capacity(50);
    for i in 0..50 {
        let id = seed_chunk(&rec, &format!("doc {i}"));
        // descending score so the truncation keeps the top-30
        hits.push(vh(&id, 1.0 - (i as f32) * 0.01));
    }
    rec.set_vector_hits(hits);

    let (storage, keyword, embedder) = build_ctx(rec.clone());
    let ctx = QueryContext::new(Query::text("q"), embedder, storage, keyword);

    let recorder = Arc::new(RecordingReranker::new());
    let root = Vector::new("chunks", 50).rerank(recorder.clone() as Arc<dyn Reranker>);
    let raw = root.retrieve(&ctx).await.unwrap();

    assert_eq!(raw.len(), 30, "rerank must truncate to k_in=30 before calling reranker");
    let received_count = recorder.received.lock().len();
    assert_eq!(received_count, 30, "reranker MUST receive exactly 30 candidates");
}

#[tokio::test]
async fn rerank_partial_hydrates_text() {
    let rec = Arc::new(RecordingStorage::new());
    // Seed chunks with distinct text bodies so we can prove the partial
    // hydration step actually fetched the right text per id.
    let id_x = seed_chunk(&rec, "the quick brown fox");
    let id_y = seed_chunk(&rec, "jumps over the lazy dog");
    rec.set_vector_hits(vec![vh(&id_x, 0.9), vh(&id_y, 0.8)]);

    let (storage, keyword, embedder) = build_ctx(rec.clone());
    let ctx = QueryContext::new(Query::text("q"), embedder, storage, keyword);

    let recorder = Arc::new(RecordingReranker::new());
    let root = Vector::new("chunks", 30).rerank(recorder.clone() as Arc<dyn Reranker>);
    let _ = root.retrieve(&ctx).await.unwrap();

    let received = recorder.received.lock().clone();
    assert_eq!(received.len(), 2);
    let by_id: HashMap<Vec<u8>, String> =
        received.into_iter().map(|c| (c.id, c.text)).collect();
    assert_eq!(by_id.get(&id_x).unwrap(), "the quick brown fox");
    assert_eq!(by_id.get(&id_y).unwrap(), "jumps over the lazy dog");
}

#[tokio::test]
async fn rerank_validates_doc_count() {
    // A buggy reranker that returns FEWER docs than it received MUST surface
    // RetrieveError::OperatorFailed (T-02-03-04 mitigation).
    struct DroppingReranker;
    #[async_trait]
    impl Reranker for DroppingReranker {
        async fn rerank(
            &self,
            _q: &str,
            mut docs: Vec<RerankCandidate>,
        ) -> Result<Vec<RerankCandidate>, LunarisError> {
            docs.pop(); // drop one — buggy / spoofed
            Ok(docs)
        }
        fn applies(&self) -> bool {
            true
        }
    }

    let rec = Arc::new(RecordingStorage::new());
    let id_a = seed_chunk(&rec, "alpha");
    let id_b = seed_chunk(&rec, "beta");
    rec.set_vector_hits(vec![vh(&id_a, 0.9), vh(&id_b, 0.7)]);

    let (storage, keyword, embedder) = build_ctx(rec.clone());
    let ctx = QueryContext::new(Query::text("q"), embedder, storage, keyword);

    let root = Vector::new("chunks", 30).rerank(Arc::new(DroppingReranker));
    let res = root.retrieve(&ctx).await;
    let err = res.expect_err("dropping reranker MUST surface OperatorFailed");
    let msg = format!("{err}");
    assert!(msg.contains("reranker returned"), "must mention doc-count mismatch; got: {msg}");
}

#[tokio::test]
async fn rerank_preserves_degraded_flag_through_rerank_pass() {
    // RawHits with degraded=true (e.g., from upstream degraded_fallback) MUST
    // remain degraded after the rerank pass — the rerank pass MUST NOT clear
    // the flag.

    // Custom upstream that emits a single RawHit with degraded=true.
    struct DegradedSource(Vec<u8>);
    #[async_trait]
    impl Retriever for DegradedSource {
        async fn retrieve(&self, _ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
            Ok(vec![RawHit {
                id: self.0.clone(),
                score: 0.5,
                rerank_applied: false,
                degraded: true,
                metadata: json!({}),
                source_op: SourceOp::Vector,
            }])
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    let rec = Arc::new(RecordingStorage::new());
    let id_a = seed_chunk(&rec, "alpha");
    let (storage, keyword, embedder) = build_ctx(rec.clone());
    let ctx = QueryContext::new(Query::text("q"), embedder, storage, keyword);

    let upstream: Box<dyn Retriever> = Box::new(DegradedSource(id_a.clone()));
    let root = lunaris_retrieve::rerank(upstream, Arc::new(NoopReranker));
    let raw = root.retrieve(&ctx).await.unwrap();
    assert_eq!(raw.len(), 1);
    assert!(raw[0].degraded, "rerank MUST preserve upstream degraded=true");
}
