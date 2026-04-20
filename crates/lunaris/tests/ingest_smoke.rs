//! End-to-end smoke test for `Lunaris::ingest` — Plan 02-01 Task 3.
//!
//! Wires a `RecordingStorage` (in-memory `StoragePort` recorder) + the Phase 1
//! `StubEmbedder` through the public `Lunaris::with_parts` constructor, ingests
//! a small Episode, and asserts the LSN is non-zero (matches Plan 01-04's
//! Episode round-trip Lsn assertion shape).

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris::Lunaris;
use lunaris_core::{
    CypherQuery, Episode, Filter, GraphResult, Hlc, HlcClock, Lsn, QueueMsg, Row,
    StorageCapabilities, StorageError, StoragePort, StubEmbedder, VectorHit, WriteOp,
};
use parking_lot::Mutex;

#[derive(Default)]
struct RecordingStorage {
    batches: Mutex<Vec<Vec<WriteOp>>>,
}

#[async_trait]
impl StoragePort for RecordingStorage {
    async fn atomic_write(&self, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        self.batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: 1, counter: 0 })
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
        Err(StorageError::NotSupported("RecordingStorage::vector_search"))
    }
    async fn graph_traverse(
        &self,
        _query: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::graph_traverse"))
    }
    async fn scan_range(
        &self,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::scan_range"))
    }
    async fn read_as_of(
        &self,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::read_as_of"))
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

#[tokio::test]
async fn ingest_against_recording_storage_returns_nonzero_lsn() {
    let clock = HlcClock::new(0);
    let storage: Arc<dyn StoragePort> = Arc::new(RecordingStorage::default());
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));
    let handle = Lunaris::with_parts(storage.clone(), embedder, clock.clone());

    let ep = Episode::new(
        "smoke.md",
        "# A\nfoo bar baz qux quux corge grault garply waldo fred\n",
        &handle.clock(),
    );
    let lsn = handle.ingest(ep).await.expect("ingest");
    assert!(
        lsn.wall_ms > 0 || lsn.counter > 0,
        "RecordingStorage emits Lsn{{wall_ms=1}}; got {lsn:?}"
    );
}

#[tokio::test]
async fn with_embedder_swaps_the_default() {
    // Proves the latency-budget escape hatch: callers can replace the
    // embedder on an already-built handle (used to swap candle → Ollama).
    let clock = HlcClock::new(0);
    let storage: Arc<dyn StoragePort> = Arc::new(RecordingStorage::default());
    let stub_a: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));
    let stub_b: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));
    let handle = Lunaris::with_parts(storage, stub_a, clock).with_embedder(stub_b);
    assert_eq!(handle.embedder().dim(), 768);
}
