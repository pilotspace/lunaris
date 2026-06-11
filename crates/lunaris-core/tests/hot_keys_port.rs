//! ADD task `hotkeys-observability` — port-level red suite (contract FROZEN
//! @ v1, 2026-06-11). No live backend needed. `StoragePort::hot_keys` is an
//! additive default method (queue_depth precedent): backends without a
//! hot-key sketch return `NotSupported("hot_keys_unsupported: …")` and the
//! server poller goes quiet after one warn.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::scope::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::capabilities::StorageCapabilities;
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, HotKey, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};

/// Minimal port keeping the DEFAULT `hot_keys`. Nothing else is exercised.
struct DefaultHotKeysPort;

#[async_trait]
impl StoragePort for DefaultHotKeysPort {
    async fn atomic_write(&self, _: &Scope, _: &[WriteOp]) -> Result<Lsn, StorageError> {
        unreachable!("not exercised")
    }
    #[allow(clippy::too_many_arguments)]
    async fn vector_search(
        &self,
        _: &Scope,
        _: &str,
        _: &[f32],
        _: usize,
        _: Option<&Filter>,
        _: Option<Hlc>,
        _: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        unreachable!("not exercised")
    }
    async fn graph_traverse(
        &self,
        _: &Scope,
        _: &CypherQuery,
        _: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        unreachable!("not exercised")
    }
    async fn scan_range(
        &self,
        _: &Scope,
        _: &[u8],
        _: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        unreachable!("not exercised")
    }
    async fn read_as_of(
        &self,
        _: &Scope,
        _: &[u8],
        _: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        unreachable!("not exercised")
    }
    async fn publish(&self, _: &Scope, _: &str, _: u16, _: Bytes) -> Result<u64, StorageError> {
        unreachable!("not exercised")
    }
    async fn subscribe(
        &self,
        _: &Scope,
        _: &str,
        _: &str,
        _: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        unreachable!("not exercised")
    }
    fn capabilities(&self) -> StorageCapabilities {
        unreachable!("not exercised")
    }
}

/// §2 — a backend without a hot-key sketch returns the named NotSupported
/// code through the default trait body, NOT a Backend error or a panic.
/// The method is scope-less by design (operator/server-global view): if a
/// future refactor adds a Scope parameter this stops compiling — that is a
/// contract change and must go back through SPECIFY.
#[tokio::test]
async fn default_hot_keys_is_not_supported() {
    let port = DefaultHotKeysPort;
    let err = port.hot_keys(128).await.expect_err("default body must be NotSupported");
    match err {
        StorageError::NotSupported(msg) => {
            assert!(msg.contains("hot_keys_unsupported"), "must carry the named code, got: {msg}");
        }
        other => panic!("expected NotSupported(hot_keys_unsupported…), got {other:?}"),
    }
}

/// HotKey is a plain serde-able data carrier (poller / future SDK surface).
#[test]
fn hot_key_serde_round_trip() {
    let hk = HotKey { key: b"lunaris:_dev_:fact:01H".to_vec(), sampled_count: 42 };
    let json = serde_json::to_string(&hk).expect("serialize");
    let back: HotKey = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, hk);
}
