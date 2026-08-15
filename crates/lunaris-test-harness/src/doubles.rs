//! Port doubles — for the assertions that are ABOUT a capability, not about a
//! substrate.
//!
//! ## Why these exist
//!
//! Nine tests in the workspace were pinned to the embedded SQLite backend
//! because their subject was something Moon does differently: "what happens
//! when the backend declares no native queue", "what happens when
//! `keyword_search` is `NotSupported`". Through 0.6.x the way to express that
//! was to open the other backend and let its real capabilities speak.
//!
//! Two backends made that convenient and dishonest. A test named
//! `handover_on_memory_backend_skips_no_queue` reads as a claim about
//! `memory://`, but the code path it exercises branches on ONE bool —
//! `capabilities().queue_native`. Pinning it to a whole second storage engine
//! coupled a three-line guard to a SQLite build, and the coupling is what made
//! the deletion expensive.
//!
//! [`PortWithCaps`] is the honest form: a real Moon underneath, one capability
//! bit overridden, and the test says exactly which bit it is about.
//!
//! ## Why a decorator and not a mock
//!
//! A hand-rolled mock returning `Ok(())` everywhere would let a handler pass by
//! never reaching storage at all. The decorator forwards **every** method to a
//! live backend, so the guard under test is the only thing standing between the
//! call and a real write. Every trait method is forwarded explicitly, including
//! the ones with default bodies: a defaulted method left unforwarded would
//! silently answer from the trait instead of the inner port, which is precisely
//! the class of bug this module is supposed to make impossible.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::scope::Scope;
use lunaris_core::storage::capabilities::StorageCapabilities;
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::port::{MaintenanceHint, StoragePort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphDecay, GraphResult, HotKey, Lsn, NavigateHit, NavigateSpec, QueueMsg,
    Row, ScopePage, VectorHit, WriteOp,
};

/// A live `StoragePort` with its declared [`StorageCapabilities`] rewritten.
///
/// Everything else is forwarded verbatim to the inner port, so a handler that
/// gets past the capability guard still talks to a real store.
pub struct PortWithCaps {
    inner: Arc<dyn StoragePort>,
    caps: StorageCapabilities,
}

impl std::fmt::Debug for PortWithCaps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortWithCaps").field("caps", &self.caps).finish_non_exhaustive()
    }
}

impl PortWithCaps {
    /// Wrap `inner`, applying `edit` to the capabilities it reports.
    ///
    /// The starting point is the inner port's OWN capabilities, so a test that
    /// clears one bit still inherits the truth about every other — a fixture
    /// cannot accidentally claim a graph the substrate does not have.
    #[must_use]
    pub fn new(inner: Arc<dyn StoragePort>, edit: impl FnOnce(&mut StorageCapabilities)) -> Self {
        let mut caps = inner.capabilities();
        edit(&mut caps);
        Self { inner, caps }
    }

    /// The common case: a backend that declares no native queue.
    ///
    /// This is what the four `queue_native == false` pins were really asking
    /// for when they asked for `memory://`.
    #[must_use]
    pub fn without_queue(inner: Arc<dyn StoragePort>) -> Self {
        Self::new(inner, |c| c.queue_native = false)
    }
}

#[async_trait]
impl StoragePort for PortWithCaps {
    async fn atomic_write(&self, scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        self.inner.atomic_write(scope, ops).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn vector_search(
        &self,
        scope: &Scope,
        index: &str,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
        rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        self.inner.vector_search(scope, index, query, k, filter, as_of, rerank).await
    }

    async fn graph_traverse(
        &self,
        scope: &Scope,
        query: &CypherQuery,
        as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        self.inner.graph_traverse(scope, query, as_of).await
    }

    async fn graph_traverse_decayed(
        &self,
        scope: &Scope,
        query: &CypherQuery,
        as_of: Option<Hlc>,
        decay: Option<&GraphDecay>,
    ) -> Result<GraphResult, StorageError> {
        self.inner.graph_traverse_decayed(scope, query, as_of, decay).await
    }

    async fn vector_navigate(
        &self,
        scope: &Scope,
        index: &str,
        query: &[f32],
        k: usize,
        spec: &NavigateSpec,
    ) -> Result<Vec<NavigateHit>, StorageError> {
        self.inner.vector_navigate(scope, index, query, k, spec).await
    }

    async fn hot_keys(&self, count: usize) -> Result<Vec<HotKey>, StorageError> {
        self.inner.hot_keys(count).await
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        self.inner.health_check().await
    }

    async fn scan_range(
        &self,
        scope: &Scope,
        prefix: &[u8],
        as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        self.inner.scan_range(scope, prefix, as_of).await
    }

    async fn read_as_of(
        &self,
        scope: &Scope,
        key: &[u8],
        as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        self.inner.read_as_of(scope, key, as_of).await
    }

    fn supports_historical_kv_reads(&self) -> bool {
        self.inner.supports_historical_kv_reads()
    }

    async fn publish(
        &self,
        scope: &Scope,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        self.inner.publish(scope, topic, partition, payload).await
    }

    async fn subscribe(
        &self,
        scope: &Scope,
        group: &str,
        topic: &str,
        partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        self.inner.subscribe(scope, group, topic, partition).await
    }

    async fn queue_depth(
        &self,
        scope: &Scope,
        topic: &str,
        partition: u16,
    ) -> Result<u64, StorageError> {
        self.inner.queue_depth(scope, topic, partition).await
    }

    async fn list_scopes(
        &self,
        prefix: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ScopePage, StorageError> {
        self.inner.list_scopes(prefix, limit, cursor).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn invalidate_range(
        &self,
        scope: &Scope,
        index: &str,
        node_id_field: &str,
        node_id_value: &str,
        hlc_wall_field: &str,
        hlc_wall_lo_inclusive: i64,
        hlc_wall_hi_inclusive: i64,
    ) -> Result<u64, StorageError> {
        self.inner
            .invalidate_range(
                scope,
                index,
                node_id_field,
                node_id_value,
                hlc_wall_field,
                hlc_wall_lo_inclusive,
                hlc_wall_hi_inclusive,
            )
            .await
    }

    /// The one method that is NOT a forward — the whole point of the type.
    fn capabilities(&self) -> StorageCapabilities {
        self.caps.clone()
    }

    async fn lookup_by_dedupe_key(
        &self,
        scope: &Scope,
        dedupe_key: &str,
    ) -> Result<Option<Lsn>, StorageError> {
        self.inner.lookup_by_dedupe_key(scope, dedupe_key).await
    }

    async fn insert_dedupe_key(
        &self,
        scope: &Scope,
        dedupe_key: &str,
        lsn: Lsn,
    ) -> Result<(), StorageError> {
        self.inner.insert_dedupe_key(scope, dedupe_key, lsn).await
    }

    async fn maintenance_hint(
        &self,
        scope: &Scope,
        hint: MaintenanceHint,
    ) -> Result<(), StorageError> {
        self.inner.maintenance_hint(scope, hint).await
    }
}

/// A `KeywordPort` that refuses every query with `NotSupported`.
///
/// The deleted embedded backend had no BM25 index, so several tests asserted
/// the retrieval layer's DEGRADED vector-only path by opening it. Moon has a
/// real `FT.SEARCH`, which makes those assertions unreachable through a
/// backend — the degrade path has to be requested directly, and this is the
/// direct request.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoKeywordSearch;

#[async_trait]
impl KeywordPort for NoKeywordSearch {
    async fn keyword_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Err(StorageError::NotSupported(
            "keyword_search unsupported: test double pinning the vector-only degrade path",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_test_storage;

    /// The decorator must inherit truth and change exactly one bit. If it
    /// started from `Default::default()` instead of the inner port's own
    /// capabilities, a fixture could silently claim (or disclaim) a graph.
    #[tokio::test]
    async fn without_queue_clears_one_bit_and_inherits_the_rest() {
        let storage = open_test_storage().await;
        let real = storage.port().capabilities();
        assert!(real.queue_native, "an ephemeral Moon must have a native queue to begin with");

        let wrapped = PortWithCaps::without_queue(storage.port());
        let caps = wrapped.capabilities();
        assert!(!caps.queue_native, "the bit under test must be cleared");
        assert_eq!(caps.graph_native, real.graph_native, "every other bit must be inherited");
        assert_eq!(caps.native_rrf, real.native_rrf);
        assert_eq!(caps.bi_temporal_native, real.bi_temporal_native);
    }

    /// Forwarding is the load-bearing half: a handler that gets past the
    /// rewritten guard must reach the real store, not a stub that says yes.
    #[tokio::test]
    async fn every_other_call_reaches_the_inner_port() {
        let storage = open_test_storage().await;
        let wrapped = PortWithCaps::without_queue(storage.port());
        wrapped.health_check().await.expect("health_check must reach the live Moon");

        let scope = Scope::new("doubles-forwarding").unwrap();
        let lsn = wrapped
            .atomic_write(
                &scope,
                &[WriteOp::KvPut {
                    key: b"lunaris:doubles-forwarding:kv:x".to_vec(),
                    value: b"v".to_vec(),
                }],
            )
            .await
            .expect("write must reach the live Moon");
        assert!(lsn.wall_ms > 0, "a forwarded write must return the backend's real LSN");
    }

    #[tokio::test]
    async fn no_keyword_search_refuses_explicitly() {
        let scope = Scope::new("doubles-nokeyword").unwrap();
        let err = NoKeywordSearch
            .keyword_search(&scope, "chunks", "anything", 5, None, None)
            .await
            .expect_err("the double must refuse");
        assert!(matches!(err, StorageError::NotSupported(_)), "got {err:?}");
    }
}
