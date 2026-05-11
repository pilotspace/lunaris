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
use serde_json;

// ── Fixtures ──────────────────────────────────────────────────────────────────

/// Recording storage that captures every `atomic_write` batch for inspection.
#[derive(Default)]
struct RecordingStorage {
    batches: Mutex<Vec<Vec<WriteOp>>>,
}

#[async_trait]
impl StoragePort for RecordingStorage {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        self.batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: 1, counter: self.batches.lock().len() as u32 })
    }
    async fn vector_search(
        &self,
        _scope: &lunaris_core::Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::vector_search"))
    }
    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        _query: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::graph_traverse"))
    }
    async fn scan_range(
        &self,
        _scope: &lunaris_core::Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::scan_range"))
    }
    async fn read_as_of(
        &self,
        _scope: &lunaris_core::Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::read_as_of"))
    }
    async fn publish(
        &self,
        _scope: &lunaris_core::Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        // Fire-and-forget — silently succeed so ingest doesn't fail on
        // the consolidate-queue publish.
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
            // Episode keys start with "lunaris:episode:"
            if key.starts_with(b"lunaris:episode:") {
                if let Ok(ep) = serde_json::from_slice::<Episode>(value) {
                    return Some(ep.scope);
                }
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
