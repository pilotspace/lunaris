//! Tower middleware composition test (RETRIEVE-09).
//!
//! Proves the retriever IS a `tower::Service<Query, Response = Vec<Hit>>` and is
//! wrappable in `ServiceBuilder::new().rate_limit(..).timeout(..).service(retriever)`
//! per blueprint §8.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    Embedder, Hlc, LunarisError, StorageCapabilities, StorageError, StoragePort, StubEmbedder,
};
use lunaris_retrieve::{Query, RetrievalService, Vector};
use parking_lot::Mutex;
use tower::{Service, ServiceBuilder, ServiceExt};

/// Minimal storage that returns canned hits — same shape as `dsl_compose::RecordingStorage`
/// but trimmed to the tower-test path.
#[derive(Default)]
struct TowerStorage {
    vector_hits: Mutex<Vec<VectorHit>>,
}

impl TowerStorage {
    fn with_hits(hits: Vec<VectorHit>) -> Self {
        Self { vector_hits: Mutex::new(hits) }
    }
}

#[async_trait]
impl StoragePort for TowerStorage {
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
        _scope: &lunaris_core::Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("TowerStorage::graph_traverse"))
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
        Err(StorageError::NotSupported("TowerStorage::publish"))
    }
    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("TowerStorage::subscribe"))
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
impl KeywordPort for TowerStorage {
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

#[tokio::test]
async fn wraps_in_tower_service_builder() {
    let store = Arc::new(TowerStorage::with_hits(vec![VectorHit {
        id: b"a".to_vec(),
        score: 0.9,
        rerank_applied: false,
        metadata: serde_json::Value::Null,
    }]));
    let storage: Arc<dyn StoragePort> = store.clone();
    let keyword: Arc<dyn KeywordPort> = store.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));

    let root: Arc<dyn lunaris_retrieve::Retriever> = Arc::new(Vector::new("chunks", 30));
    let svc = RetrievalService::new(root, embedder, storage, keyword);

    // Compose with rate-limit + timeout. The compile is the proof — every
    // layer on the chain has to type-check the inner service shape.
    let mut wrapped = ServiceBuilder::new()
        .rate_limit(10, Duration::from_secs(1))
        .timeout(Duration::from_secs(5))
        .service(svc);

    let resp = wrapped.ready().await.unwrap().call(Query::text("foo")).await;
    // Hydration culls the hit (no chunk row); the test verifies the call
    // round-trip works without error.
    let _hits = resp.expect("tower call must succeed");
}

#[tokio::test]
async fn retrieval_service_is_clone_so_tower_buffer_works() {
    // Sanity: RetrievalService is Clone so wrapping in tower::Buffer works.
    let store = Arc::new(TowerStorage::with_hits(vec![]));
    let storage: Arc<dyn StoragePort> = store.clone();
    let keyword: Arc<dyn KeywordPort> = store.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let root: Arc<dyn lunaris_retrieve::Retriever> = Arc::new(Vector::new("chunks", 30));
    let svc = RetrievalService::new(root, embedder, storage, keyword);
    let _clone = svc.clone();
}

// LunarisError used so the import isn't dead — surfaces in tower layer error
// types via `Service::Error = LunarisError` (with tower wrapping it inside its
// own error envelope under the timeout / rate_limit layers).
#[allow(dead_code)]
fn _err_marker() -> LunarisError {
    LunarisError::Storage(StorageError::Backend("marker".into()))
}
