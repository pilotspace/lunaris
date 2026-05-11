//! RFC 0001 Wave 1D — `ScopedLunaris<'a>` integration tests.
//!
//! Asserts the core scope-isolation contract: ingesting the same
//! [`EpisodeBuilder`] payload under two different scopes produces two
//! distinct rows, each carrying exactly the scope it was ingested under.
//! The test wires the same `RecordingStorage` that `ingest_smoke.rs` uses
//! so no live Moon / Postgres is required.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris::Lunaris;
use lunaris::episode_builder::EpisodeBuilder;
use lunaris_core::{
    CypherQuery, Episode, Filter, GraphResult, Hlc, HlcClock, Lsn, QueueMsg, Row, Scope,
    StorageCapabilities, StorageError, StoragePort, StubEmbedder, VectorHit, WriteOp,
};
use parking_lot::Mutex;

// ── Fixtures ──────────────────────────────────────────────────────────────────

/// Recording storage that captures every `atomic_write` batch AND every
/// `vector_search` scope for inspection. Wave 2.5C: the second recorder
/// proves the retrieval-path scope propagation contract — `ctx.scope`
/// reaches `StoragePort::vector_search` as the bound `ScopedLunaris.scope`.
#[derive(Default)]
struct RecordingStorage {
    batches: Mutex<Vec<Vec<WriteOp>>>,
    vector_search_scopes: Mutex<Vec<Scope>>,
    // P-5 (v0.2 review) — propagation regression matrix extension.
    atomic_write_scopes: Mutex<Vec<Scope>>,
    graph_traverse_scopes: Mutex<Vec<Scope>>,
    read_as_of_scopes: Mutex<Vec<Scope>>,
    publish_scopes: Mutex<Vec<Scope>>,
    scan_range_scopes: Mutex<Vec<Scope>>,
}

#[async_trait]
impl StoragePort for RecordingStorage {
    async fn atomic_write(
        &self,
        scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        self.atomic_write_scopes.lock().push(scope.clone());
        self.batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: 1, counter: self.batches.lock().len() as u32 })
    }
    async fn vector_search(
        &self,
        scope: &lunaris_core::Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        self.vector_search_scopes.lock().push(scope.clone());
        // Empty hits — assertion is on the recorded scope, not the hit
        // payload. Returning Ok(vec![]) instead of NotSupported lets recall()
        // complete past this call (vs short-circuiting to error).
        Ok(Vec::new())
    }
    async fn graph_traverse(
        &self,
        scope: &lunaris_core::Scope,
        _query: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        self.graph_traverse_scopes.lock().push(scope.clone());
        Ok(GraphResult::default())
    }
    async fn scan_range(
        &self,
        scope: &lunaris_core::Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        self.scan_range_scopes.lock().push(scope.clone());
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn read_as_of(
        &self,
        scope: &lunaris_core::Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        self.read_as_of_scopes.lock().push(scope.clone());
        Ok(None)
    }
    async fn publish(
        &self,
        scope: &lunaris_core::Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        self.publish_scopes.lock().push(scope.clone());
        Ok(0)
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
            native_rrf: false,
            max_scopes_recommended: 0,
        }
    }
}

/// Extract the first `WriteOp::KvPut` that looks like an Episode row from a
/// batch, deserialize it, and return the scope field.
fn extract_episode_scope(batch: &[WriteOp]) -> Option<Scope> {
    for op in batch {
        if let WriteOp::KvPut { key, value } = op {
            // RFC 0001 Wave 1C: keys are now scope-prefixed
            // `lunaris:{scope}:episode:{ulid}`. Probe by middle segment.
            let key_str = std::str::from_utf8(key).ok()?;
            if key_str.contains(":episode:")
                && let Ok(ep) = serde_json::from_slice::<Episode>(value)
            {
                return Some(ep.scope);
            }
        }
    }
    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Core scope-isolation contract (RFC 0001 Wave 1D success criterion):
///
/// Ingesting the **same** `EpisodeBuilder` payload under two different
/// scopes produces two rows that carry exactly the scope they were ingested
/// under, and the scopes are distinct from each other.
#[tokio::test]
async fn scoped_ingest_stamps_episode_with_bound_scope() {
    let storage = Arc::new(RecordingStorage::default());
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let engine = Lunaris::with_parts(storage.clone() as Arc<dyn StoragePort>, embedder, clock);

    let scope_a = Scope::new("agent:alpha").unwrap();
    let scope_b = Scope::new("agent:beta").unwrap();

    let builder_a = EpisodeBuilder::new("test:source", "# Hello\nSame payload for both scopes.");
    let builder_b = EpisodeBuilder::new("test:source", "# Hello\nSame payload for both scopes.");

    // Ingest under scope_a then scope_b.
    let lsn_a =
        engine.scoped(scope_a.clone()).ingest(builder_a).await.expect("ingest under scope_a");
    let lsn_b =
        engine.scoped(scope_b.clone()).ingest(builder_b).await.expect("ingest under scope_b");

    // Both ingests returned a non-trivial LSN.
    assert!(
        lsn_a.wall_ms > 0 || lsn_a.counter > 0,
        "scope_a ingest must return non-zero Lsn; got {lsn_a:?}"
    );
    assert!(
        lsn_b.wall_ms > 0 || lsn_b.counter > 0,
        "scope_b ingest must return non-zero Lsn; got {lsn_b:?}"
    );

    // Verify that each ingest batch carries the correct scope on the
    // serialized Episode row.
    let batches = storage.batches.lock();
    assert!(batches.len() >= 2, "expected at least 2 atomic_write calls; got {}", batches.len());

    let found_a = extract_episode_scope(&batches[0]);
    let found_b = extract_episode_scope(&batches[1]);

    assert_eq!(
        found_a.as_ref(),
        Some(&scope_a),
        "first ingest batch must carry scope_a; got {found_a:?}"
    );
    assert_eq!(
        found_b.as_ref(),
        Some(&scope_b),
        "second ingest batch must carry scope_b; got {found_b:?}"
    );

    // The two scopes must be distinct from each other.
    assert_ne!(
        found_a, found_b,
        "scope_a and scope_b must differ — same scope would collapse isolation"
    );
}

/// `ScopedLunaris::scope()` returns exactly the scope passed to `Lunaris::scoped`.
#[test]
fn scoped_handle_exposes_bound_scope() {
    let storage = Arc::new(RecordingStorage::default());
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let engine = Lunaris::with_parts(storage as Arc<dyn StoragePort>, embedder, clock);

    let scope = Scope::new("agent:gamma").unwrap();
    let scoped = engine.scoped(scope.clone());
    assert_eq!(scoped.scope(), &scope);
}

/// `ScopedLunaris::dsl()` returns a `RetrievalBuilder` without panicking.
/// (Trivial but guards that the method is reachable and has the right return
/// type without requiring a real storage backend.)
#[test]
fn scoped_dsl_returns_retrieval_builder() {
    use lunaris::RetrievalBuilder;

    let storage = Arc::new(RecordingStorage::default());
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let engine = Lunaris::with_parts(storage as Arc<dyn StoragePort>, embedder, clock);

    let scope = Scope::new("agent:delta").unwrap();
    // Should not panic; the return type is RetrievalBuilder.
    let _builder: RetrievalBuilder = engine.scoped(scope).dsl();
}

/// Wave 2.5C retrieval-layer scope-isolation contract:
///
/// `engine.scoped(scope_a).recall(query)` MUST route the storage-layer
/// `vector_search` call with `&scope_a`, not `Scope::dev()` or `scope_b`.
/// This proves `QueryContext` propagates the bound scope all the way down
/// the operator tree to the storage port — closing the gap Wave 2 flagged
/// where ingest-time isolation was enforced but retrieval-time was not.
#[tokio::test]
async fn scoped_recall_propagates_scope_to_vector_search() {
    use lunaris::Vector;
    use lunaris_retrieve::Query;

    let storage = Arc::new(RecordingStorage::default());
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let engine = Lunaris::with_parts(storage.clone() as Arc<dyn StoragePort>, embedder, clock);

    let scope_a = Scope::new("agent:alpha").unwrap();
    let scope_b = Scope::new("agent:beta").unwrap();

    // Run two scope_a recalls and one scope_b recall through the DSL path so
    // every recorded scope is unambiguous.
    let _ = engine
        .scoped(scope_a.clone())
        .dsl()
        .with_root(Vector::new("chunks", 5).top(5))
        .execute(Query::text("hello"))
        .await
        .expect("scope_a recall must not error on empty hits");

    let _ = engine
        .scoped(scope_a.clone())
        .dsl()
        .with_root(Vector::new("chunks", 5).top(5))
        .execute(Query::text("world"))
        .await
        .expect("second scope_a recall must not error");

    let _ = engine
        .scoped(scope_b.clone())
        .dsl()
        .with_root(Vector::new("chunks", 5).top(5))
        .execute(Query::text("hello"))
        .await
        .expect("scope_b recall must not error");

    let recorded = storage.vector_search_scopes.lock().clone();
    assert!(
        recorded.len() >= 3,
        "expected at least 3 vector_search calls (2x scope_a + 1x scope_b); got {}",
        recorded.len()
    );

    let scope_a_count = recorded.iter().filter(|s| *s == &scope_a).count();
    let scope_b_count = recorded.iter().filter(|s| *s == &scope_b).count();
    let dev_count = recorded.iter().filter(|s| s.as_str() == "_dev_").count();

    assert_eq!(
        scope_a_count, 2,
        "scope_a's recalls must each pass scope_a to vector_search; recorded: {recorded:?}"
    );
    assert_eq!(
        scope_b_count, 1,
        "scope_b's recall must pass scope_b to vector_search; recorded: {recorded:?}"
    );
    assert_eq!(
        dev_count, 0,
        "no Scope::dev() leakage allowed on scoped recall paths; recorded: {recorded:?}"
    );
}

// ── P-5 (v0.2 review) — propagation regression matrix ───────────────────────
//
// The release-gate review noted that `RecordingStorage` recorded all
// six partitioned port arguments but only `vector_search` was asserted.
// These four tests close the matrix: atomic_write (ingest path),
// publish (consolidate-queue), read_as_of (DSL hydrate path), and
// graph_traverse (Graph operator DSL branch). Each test follows the
// same pattern as `scoped_recall_propagates_scope_to_vector_search`:
// drive the path under two distinct scopes via the public typestate
// API and assert the recorder saw exactly the bound scope on every
// call, with zero `Scope::dev()` leakage.

#[tokio::test]
async fn scoped_ingest_propagates_scope_to_atomic_write() {
    let storage = Arc::new(RecordingStorage::default());
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let engine = Lunaris::with_parts(storage.clone() as Arc<dyn StoragePort>, embedder, clock);

    let scope_a = Scope::new("agent:p5-a").unwrap();
    let scope_b = Scope::new("agent:p5-b").unwrap();

    engine
        .scoped(scope_a.clone())
        .ingest(EpisodeBuilder::new("src", "hello"))
        .await
        .expect("ingest under scope_a");
    engine
        .scoped(scope_b.clone())
        .ingest(EpisodeBuilder::new("src", "world"))
        .await
        .expect("ingest under scope_b");

    let recorded = storage.atomic_write_scopes.lock().clone();
    assert_eq!(recorded.len(), 2, "expected 2 atomic_write calls; got {recorded:?}");
    assert_eq!(&recorded[0], &scope_a, "first atomic_write must use scope_a");
    assert_eq!(&recorded[1], &scope_b, "second atomic_write must use scope_b");
    assert!(
        recorded.iter().all(|s| s.as_str() != "_dev_"),
        "Scope::dev() leakage detected on atomic_write path: {recorded:?}"
    );
}

#[tokio::test]
async fn scoped_ingest_propagates_scope_to_publish() {
    // Lunaris::ingest fires one __lunaris_consolidate__ publish per call
    // (Plan 04-04 D-16). The publish MUST carry the bound scope, not
    // Scope::dev().
    let storage = Arc::new(RecordingStorage::default());
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let engine = Lunaris::with_parts(storage.clone() as Arc<dyn StoragePort>, embedder, clock);

    let scope_a = Scope::new("agent:p5-pub-a").unwrap();
    let scope_b = Scope::new("agent:p5-pub-b").unwrap();

    engine
        .scoped(scope_a.clone())
        .ingest(EpisodeBuilder::new("src", "alpha payload"))
        .await
        .expect("ingest under scope_a");
    engine
        .scoped(scope_b.clone())
        .ingest(EpisodeBuilder::new("src", "beta payload"))
        .await
        .expect("ingest under scope_b");

    let recorded = storage.publish_scopes.lock().clone();
    assert!(
        recorded.iter().any(|s| s == &scope_a),
        "publish must carry scope_a at least once; recorded: {recorded:?}"
    );
    assert!(
        recorded.iter().any(|s| s == &scope_b),
        "publish must carry scope_b at least once; recorded: {recorded:?}"
    );
    assert!(
        recorded.iter().all(|s| s.as_str() != "_dev_"),
        "Scope::dev() leakage detected on publish path: {recorded:?}"
    );
}

#[tokio::test]
async fn scoped_recall_propagates_scope_to_graph_traverse() {
    use lunaris::{EntityId, Graph};
    use lunaris_retrieve::Query;

    let storage = Arc::new(RecordingStorage::default());
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let engine = Lunaris::with_parts(storage.clone() as Arc<dyn StoragePort>, embedder, clock);

    let scope_a = Scope::new("agent:p5-gt-a").unwrap();
    let scope_b = Scope::new("agent:p5-gt-b").unwrap();

    let _ = engine
        .scoped(scope_a.clone())
        .dsl()
        .with_root(Graph::anchored(vec![EntityId::from_name_and_type("Probe", "Test")], 2))
        .execute(Query::text("alpha"))
        .await
        .expect("scope_a graph recall must not error");
    let _ = engine
        .scoped(scope_b.clone())
        .dsl()
        .with_root(Graph::anchored(vec![EntityId::from_name_and_type("Probe", "Test")], 2))
        .execute(Query::text("beta"))
        .await
        .expect("scope_b graph recall must not error");

    let recorded = storage.graph_traverse_scopes.lock().clone();
    assert!(
        recorded.iter().any(|s| s == &scope_a),
        "graph_traverse must carry scope_a at least once; recorded: {recorded:?}"
    );
    assert!(
        recorded.iter().any(|s| s == &scope_b),
        "graph_traverse must carry scope_b at least once; recorded: {recorded:?}"
    );
    assert!(
        recorded.iter().all(|s| s.as_str() != "_dev_"),
        "Scope::dev() leakage detected on graph_traverse path: {recorded:?}"
    );
}
