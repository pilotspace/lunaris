//! DSL composition tests.
//!
//! These tests use a `RecordingRetrieval` fixture that implements both
//! `StoragePort` and `KeywordPort` so the operator tree can run end-to-end
//! against deterministic canned hits — no Moon / Postgres required.

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
    BiTemporal, Embedder, Hlc, HlcClock, StorageCapabilities, StorageError, StoragePort,
    StubEmbedder,
};
use lunaris_retrieve::{
    Keyword, Plan, Query, RetrievalBuilder, SourceOp, Vector, filter_str, plan_query,
};
use parking_lot::Mutex;
use serde_json::json;

// ============================================================ Fixtures

type VectorCallRecord = (String, usize, bool, Option<Hlc>);
type KeywordCallRecord = (String, String, usize, bool, Option<Hlc>);

/// Recording storage that returns canned hits and records every call.
#[derive(Default)]
struct RecordingStorage {
    /// canned VectorHits returned from the next vector_search call
    vector_hits: Mutex<Vec<VectorHit>>,
    /// canned KeywordHits returned from the next keyword_search call
    keyword_hits: Mutex<Vec<KeywordHit>>,
    /// records `(index, k, filter.is_some(), as_of)` for every vector_search
    last_vector_args: Mutex<Option<VectorCallRecord>>,
    /// records `(index, query, k, filter.is_some(), as_of)` for every keyword_search
    last_keyword_args: Mutex<Option<KeywordCallRecord>>,
    /// canned chunk KV rows for hydration: key bytes -> serialized Chunk JSON
    chunks_by_key: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    /// canned episode KV rows: key bytes -> serialized Episode JSON
    episodes_by_key: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    /// Plan 03-02: canned GraphResult returned from the next graph_traverse call
    canned_graph: Mutex<GraphResult>,
    /// Plan 03-02: records every graph_traverse `(query, as_of)` for assertion
    graph_calls: Mutex<Vec<(CypherQuery, Option<Hlc>)>>,
}

impl RecordingStorage {
    fn new() -> Self {
        Self::default()
    }

    fn set_vector_hits(&self, hits: Vec<VectorHit>) {
        *self.vector_hits.lock() = hits;
    }

    fn set_keyword_hits(&self, hits: Vec<KeywordHit>) {
        *self.keyword_hits.lock() = hits;
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
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        *self.last_vector_args.lock() = Some((index.to_string(), k, filter.is_some(), as_of));
        Ok(self.vector_hits.lock().clone())
    }

    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        q: &CypherQuery,
        as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        // Plan 03-02: record the call shape (CypherQuery + as_of) for
        // assertion in tests, then return the canned GraphResult. Default
        // canned_graph is empty (GraphResult::default()) so existing tests
        // that never call graph_traverse see no behavior change.
        self.graph_calls.lock().push((q.clone(), as_of));
        Ok(self.canned_graph.lock().clone())
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
        if let Some(v) = self.episodes_by_key.lock().get(key).cloned() {
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
        Err(StorageError::NotSupported("RecordingStorage::publish"))
    }

    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
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
            // false → fuse_rrf takes the client-side path; the Moon-native
            // path requires a real MoonStorage Arc + native_rrf=true (see
            // moon_native_rrf integration test).
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
        index: &str,
        query: &str,
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        *self.last_keyword_args.lock() =
            Some((index.to_string(), query.to_string(), k, filter.is_some(), as_of));
        Ok(self.keyword_hits.lock().clone())
    }
}

fn vh(id: &[u8], score: f32) -> VectorHit {
    VectorHit { id: id.to_vec(), score, rerank_applied: false, metadata: json!({}) }
}

fn kh(id: &[u8], score: f32) -> KeywordHit {
    KeywordHit::new(id.to_vec(), score, score, json!({}))
}

fn build_ctx(
    rec: Arc<RecordingStorage>,
) -> (Arc<dyn StoragePort>, Arc<dyn KeywordPort>, Arc<dyn Embedder>) {
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    (storage, keyword, embedder)
}

// ============================================================ Tests

#[tokio::test]
async fn vector_and_keyword_compiles_and_runs() {
    let rec = Arc::new(RecordingStorage::new());
    rec.set_vector_hits(vec![vh(b"a", 0.95), vh(b"b", 0.80), vh(b"c", 0.55)]);
    rec.set_keyword_hits(vec![kh(b"b", 0.99), kh(b"d", 0.60), kh(b"a", 0.55)]);
    let (storage, keyword, embedder) = build_ctx(rec.clone());

    // Build the canonical DSL example from blueprint §8.
    let root = Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(5);

    let builder = RetrievalBuilder::new(storage, keyword, embedder).with_root(root);
    let hits = builder.execute(Query::text("brown fox")).await.unwrap();

    // Hydration drops hits without a chunk row in storage. We seeded NONE
    // so all hits get filtered. Add the canonical assertion: the operator
    // tree completes without error AND the recorded inputs match the
    // builder's wiring.
    let (vec_index, vec_k, _, _) = rec.last_vector_args.lock().clone().unwrap();
    assert_eq!(vec_index, "chunks");
    assert_eq!(vec_k, 30);
    let (kw_index, kw_query, kw_k, _, _) = rec.last_keyword_args.lock().clone().unwrap();
    assert_eq!(kw_index, "chunks");
    assert_eq!(kw_query, "brown fox");
    assert_eq!(kw_k, 30);
    // Hydration culls every hit (no chunk rows seeded), proving the cull
    // path works AND gives us a stable assertion.
    assert!(hits.is_empty(), "hydration must cull hits with no chunk row");
}

#[tokio::test]
async fn or_unions_by_id_with_max_score() {
    use lunaris_retrieve::operators::Retriever;

    // Two retrievers that overlap on id `b`. Confirm union dedupes.
    let rec_left = Arc::new(RecordingStorage::new());
    rec_left.set_vector_hits(vec![vh(b"a", 0.9), vh(b"b", 0.5)]);
    let rec_right = Arc::new(RecordingStorage::new());
    rec_right.set_vector_hits(vec![vh(b"b", 0.95), vh(b"c", 0.4)]);

    // Build two Vector operators against the SAME storage but with different
    // canned hits — easier than wiring two separate stores. Note: real or()
    // uses the same backend; tests compose against canned outputs.
    //
    // Workaround: have the SAME RecordingStorage return both sets in a
    // single call by setting both. We use the fact that vector_search
    // returns the canned set verbatim each time.
    let rec = Arc::new(RecordingStorage::new());
    // Set hits that include the union: a (0.9), b (max 0.95 from right), c (0.4)
    rec.set_vector_hits(vec![vh(b"a", 0.9), vh(b"b", 0.5), vh(b"b", 0.95), vh(b"c", 0.4)]);
    let (storage, keyword, embedder) = build_ctx(rec.clone());
    drop((rec_left, rec_right));

    let root = Vector::new("chunks", 30).or(Vector::new("chunks", 30));

    use lunaris_retrieve::QueryContext;
    let q = Query::text("query");
    let ctx = QueryContext::new(q, lunaris_core::Scope::dev(), embedder, storage, keyword);
    let raw = root.retrieve(&ctx).await.unwrap();
    // Union by id: a, b (max 0.95), c. The duplicate `b` collapses into one
    // entry with score 0.95.
    let ids: Vec<_> = raw.iter().map(|h| h.id.clone()).collect();
    assert!(ids.contains(&b"a".to_vec()));
    assert!(ids.contains(&b"b".to_vec()));
    assert!(ids.contains(&b"c".to_vec()));
    let b_hit = raw.iter().find(|h| h.id == b"b".to_vec()).unwrap();
    assert!((b_hit.score - 0.95).abs() < 1e-4, "max score must win on dedupe");
}

#[tokio::test]
async fn then_passes_a_hits_as_b_filter() {
    use lunaris_retrieve::operators::Retriever;

    let rec = Arc::new(RecordingStorage::new());
    rec.set_vector_hits(vec![vh(b"x", 0.9), vh(b"y", 0.7), vh(b"z", 0.5)]);
    let (storage, keyword, embedder) = build_ctx(rec.clone());

    let root = Vector::new("chunks", 30).then(Vector::new("chunks", 30));

    use lunaris_retrieve::QueryContext;
    let q = Query::text("query");
    let ctx = QueryContext::new(q, lunaris_core::Scope::dev(), embedder, storage, keyword);
    let _ = root.retrieve(&ctx).await.unwrap();

    // After A runs (returns x/y/z), B receives a filter built from those ids.
    let (_, _, second_filter, _) = rec.last_vector_args.lock().clone().unwrap();
    assert!(second_filter, "B must receive a non-None filter built from A's hits");
}

#[tokio::test]
async fn filter_str_parses_starts_with_canonical() {
    let f = filter_str("source LIKE 'helios:fs/%'").unwrap();
    match f {
        Filter::StartsWith { field, prefix } => {
            assert_eq!(field, "source");
            assert_eq!(prefix, "helios:fs/");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn as_of_threads_through_to_storage() {
    use lunaris_retrieve::operators::Retriever;

    let rec = Arc::new(RecordingStorage::new());
    rec.set_vector_hits(vec![vh(b"a", 0.9)]);
    let (storage, keyword, embedder) = build_ctx(rec.clone());

    use lunaris_retrieve::QueryContext;
    let stamp = Hlc { wall_ms: 12345, counter: 7, node_id: 0 };
    let mut q = Query::text("query");
    q.as_of = Some(stamp);
    let ctx = QueryContext::new(q, lunaris_core::Scope::dev(), embedder, storage, keyword);
    let v = Vector::new("chunks", 30);
    let _ = v.retrieve(&ctx).await.unwrap();

    let (_, _, _, recorded_as_of) = rec.last_vector_args.lock().clone().unwrap();
    assert_eq!(recorded_as_of, Some(stamp));
}

#[tokio::test]
async fn query_planner_picks_hybrid_for_capitalized_token() {
    assert_eq!(plan_query("what did Alice do at Acme last April?"), Plan::Hybrid);
    assert_eq!(plan_query("show me everything"), Plan::VectorOnly);
}

#[tokio::test]
async fn hydrate_returns_chunk_text_and_source() {
    use lunaris_core::primitives::{Chunk, Episode};

    let rec = Arc::new(RecordingStorage::new());
    let clock = HlcClock::new(0);
    let episode =
        Episode::new(lunaris_core::Scope::dev(), "notes.md", "raw content unused here", &clock);
    let mut chunk = Chunk::new(
        lunaris_core::Scope::dev(),
        episode.id,
        "the brown fox",
        4,
        0,
        vec!["Notes".to_string()],
        &clock,
    );
    chunk.embedding = Some(vec![0.0_f32; 768]);

    let chunk_id_bytes = chunk.id.to_bytes().to_vec();
    let chunk_key = format!("lunaris:chunk:{}", chunk.id).into_bytes();
    let episode_key = format!("lunaris:episode:{}", episode.id).into_bytes();
    rec.chunks_by_key.lock().insert(chunk_key, serde_json::to_vec(&chunk).unwrap());
    rec.episodes_by_key.lock().insert(episode_key, serde_json::to_vec(&episode).unwrap());
    // Vector hit must use the same id bytes as the chunk's ulid.
    rec.set_vector_hits(vec![vh(&chunk_id_bytes, 0.95)]);

    let (storage, keyword, embedder) = build_ctx(rec.clone());

    let root = Vector::new("chunks", 30).top(1);
    let builder = RetrievalBuilder::new(storage, keyword, embedder).with_root(root);
    let hits = builder.execute(Query::text("brown fox")).await.unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].text, "the brown fox");
    assert_eq!(hits[0].source, "notes.md");
    assert_eq!(hits[0].heading_path, vec!["Notes".to_string()]);
    assert_eq!(hits[0].source_op, SourceOp::Vector);
}

#[tokio::test]
async fn fuse_rrf_client_side_path_runs_for_non_native_backend() {
    use lunaris_retrieve::operators::Retriever;

    // RecordingStorage reports `native_rrf = false`, so fuse_rrf takes the
    // client-side path. This tests the FALLBACK is alive end-to-end.
    let rec = Arc::new(RecordingStorage::new());
    rec.set_vector_hits(vec![vh(b"a", 0.9), vh(b"b", 0.5)]);
    rec.set_keyword_hits(vec![kh(b"b", 0.95), kh(b"c", 0.4)]);
    let (storage, keyword, embedder) = build_ctx(rec.clone());

    use lunaris_retrieve::QueryContext;
    let root = Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60);
    let q = Query::text("query");
    let ctx = QueryContext::new(q, lunaris_core::Scope::dev(), embedder, storage, keyword);
    let raw = root.retrieve(&ctx).await.unwrap();

    // Three unique ids — a, b, c. b appears in both branches → top of fused.
    assert_eq!(raw.len(), 3);
    assert_eq!(raw[0].source_op, SourceOp::Fused);
    assert_eq!(raw[0].id, b"b".to_vec(), "b appears in both branches; must rank top");
}
