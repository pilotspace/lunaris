//! ADD task `ft-navigate-recall` — NavigateSpec/NavigateHit types + default-port
//! behavior (contract FROZEN @ v1, 2026-06-11). No live backend needed.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::scope::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::capabilities::StorageCapabilities;
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphDecay, GraphResult, Lsn, NavigateSpec, QueueMsg, Row, VectorHit,
    WriteOp,
};

#[test]
fn rejects_invalid_hops() {
    for bad in [0u32, 6, 100] {
        let err = NavigateSpec::new(bad).expect_err("invalid hops must be rejected");
        assert!(
            err.to_string().contains("navigate_invalid_hops"),
            "error must carry the named code, got: {err}"
        );
    }
    // Valid boundary values construct and echo through accessors.
    for good in [1u32, 2, 5] {
        let spec = NavigateSpec::new(good).expect("hops in 1..=5 are valid");
        assert_eq!(spec.hops(), good);
        assert_eq!(spec.hop_penalty(), None);
        assert_eq!(spec.decay(), None);
    }
}

#[test]
fn rejects_invalid_hop_penalty() {
    let base = NavigateSpec::new(2).expect("valid hops");
    for bad in [f64::NAN, f64::INFINITY, -0.1] {
        let err = base.with_hop_penalty(bad).expect_err("invalid penalty must be rejected");
        assert!(
            err.to_string().contains("navigate_invalid_hop_penalty"),
            "error must carry the named code, got: {err}"
        );
    }
    // 0.0 is a valid penalty (graph hits rank purely by decay/vec score).
    let zero = base.with_hop_penalty(0.0).expect("zero penalty is valid");
    assert_eq!(zero.hop_penalty(), Some(0.0));
    let typical = base.with_hop_penalty(0.25).expect("positive finite penalty");
    assert_eq!(typical.hop_penalty(), Some(0.25));
    assert_eq!(typical.hops(), 2);
}

#[test]
fn with_decay_composes_infallibly() {
    let decay = GraphDecay::new(2.0).expect("valid λ");
    let spec = NavigateSpec::new(3).expect("valid hops").with_decay(decay);
    assert_eq!(spec.decay(), Some(decay));
    assert_eq!(spec.hops(), 3);
}

/// Minimal port keeping the DEFAULT `vector_navigate`. Only `capabilities`
/// is otherwise reachable.
struct DefaultNavigatePort;

#[async_trait]
impl StoragePort for DefaultNavigatePort {
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
        test_caps()
    }
}

/// Full literal so the compiler pins every field — including the new
/// `graph_navigate_native` — at its non-Moon default.
fn test_caps() -> StorageCapabilities {
    StorageCapabilities {
        bi_temporal_native: false,
        graph_native: true,
        rerank_native: false,
        queue_native: false,
        max_vector_dim: 768,
        native_rrf: false,
        max_scopes_recommended: 0,
        cypher_dialect: Default::default(),
        graph_decay_native: false,
        graph_navigate_native: false,
    }
}

#[tokio::test]
async fn default_port_navigate_not_supported() {
    let port = DefaultNavigatePort;
    let scope = Scope::new("navigate-default").unwrap();
    let spec = NavigateSpec::new(2).unwrap();
    let err = port
        .vector_navigate(&scope, "entities", &[0.1, 0.2], 3, &spec)
        .await
        .expect_err("default impl must refuse navigate");
    assert!(matches!(err, StorageError::NotSupported(_)), "must be NotSupported, got {err:?}");
    assert!(err.to_string().contains("graph_navigate_unsupported"), "named code, got: {err}");
}

#[test]
fn capabilities_serde_default_is_false() {
    let caps = test_caps();
    let mut v = serde_json::to_value(caps).expect("serialize");
    v.as_object_mut().expect("object").remove("graph_navigate_native");
    let back: StorageCapabilities = serde_json::from_value(v).expect("old payload must parse");
    assert!(!back.graph_navigate_native, "missing field must default to false");
}
