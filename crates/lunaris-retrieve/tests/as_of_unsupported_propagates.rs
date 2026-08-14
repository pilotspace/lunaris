//! 0.6.2 task 9 — a storage backend that refuses a historical `as_of` must
//! surface that refusal to the caller, not swallow it.
//!
//! Moon's `read_as_of` now answers a historical pin with
//! `StorageError::NotSupported(_)` instead of present-time data. That is
//! only useful if the refusal survives the retrieval fan-out: `hydrate()`
//! deliberately SKIPS hits whose row is missing (`Ok(None)`) and drops rows
//! that fail to deserialize, so "swallow anything unexpected" would be an
//! easy and invisible mistake — the caller would get `200 OK` with an empty
//! hit list for a query the backend could not answer.
//!
//! This pins the whole chain the HTTP surface depends on:
//!
//! ```text
//! POST /v1/recall {as_of: <last week>}   (routes/recall.rs)
//!   → RetrievalBuilder::execute → hydrate → StoragePort::read_as_of
//!   → Err(StorageError::NotSupported(_))            ← this test
//!   → map_error                                     ← middleware/error.rs
//!   → 501 { "error": "not_supported" }
//! ```
//!
//! Storage double shape is copied from `d_as_of_pinning.rs` with one
//! change: `read_as_of` refuses instead of returning `Ok(None)`.

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    Embedder, Hlc, LunarisError, Scope, StorageCapabilities, StorageError, StoragePort,
    StubEmbedder,
};
use lunaris_retrieve::{Query, QueryContext, RawHit, RetrievalBuilder, Retriever, SourceOp};
use serde_json::json;

/// Stands in for the Moon backend after the 0.6.2 fix: every `read_as_of`
/// is a historical pin and is refused.
#[derive(Default)]
struct AsOfRefusingStorage;

#[async_trait]
impl StoragePort for AsOfRefusingStorage {
    async fn atomic_write(&self, _scope: &Scope, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
    async fn vector_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Ok(Vec::new())
    }
    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("AsOfRefusingStorage::graph_traverse"))
    }
    async fn scan_range(
        &self,
        _scope: &Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new()).boxed())
    }
    async fn read_as_of(
        &self,
        _scope: &Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Err(StorageError::NotSupported("moon_kv_as_of: historical read_as_of unsupported"))
    }
    fn supports_historical_kv_reads(&self) -> bool {
        false
    }
    async fn publish(
        &self,
        _scope: &Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("AsOfRefusingStorage::publish"))
    }
    async fn subscribe(
        &self,
        _scope: &Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("AsOfRefusingStorage::subscribe"))
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
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

#[async_trait]
impl KeywordPort for AsOfRefusingStorage {
    async fn keyword_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(Vec::new())
    }
}

/// Root operator that yields one hit, so `hydrate()` actually issues the
/// `read_as_of` that gets refused.
struct OneHitRoot;

#[async_trait]
impl Retriever for OneHitRoot {
    async fn retrieve(&self, _ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        Ok(vec![RawHit {
            id: ulid::Ulid::new().to_bytes().to_vec(),
            score: 1.0,
            rerank_applied: false,
            degraded: false,
            metadata: json!({}),
            source_op: SourceOp::Vector,
        }])
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn builder() -> RetrievalBuilder {
    let storage: Arc<AsOfRefusingStorage> = Arc::new(AsOfRefusingStorage);
    let storage_dyn: Arc<dyn StoragePort> = storage.clone();
    let keyword_dyn: Arc<dyn KeywordPort> = storage;
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    RetrievalBuilder::new(storage_dyn, keyword_dyn, embedder).with_root(OneHitRoot)
}

#[tokio::test]
async fn recall_surfaces_the_backend_as_of_refusal() {
    // A week-old pin — the shape `POST /v1/recall {"as_of": ...}` produces.
    let week_ago = Hlc::from_parts(1_000, 0, 0);
    let mut query = Query::text("what did I know then");
    query.as_of = Some(week_ago);

    let result = builder().execute(query).await;

    match result {
        Err(LunarisError::Storage(StorageError::NotSupported(_))) => {}
        Err(other) => panic!("expected the storage refusal to propagate verbatim, got {other:?}"),
        Ok(hits) => panic!(
            "recall must NOT degrade a backend's `NotSupported` refusal into a successful \
             empty result ({} hits) — the caller would read that as 'nothing matched at that \
             instant', which is exactly the false history the 0.6.2 fix removes",
            hits.len()
        ),
    }
}
