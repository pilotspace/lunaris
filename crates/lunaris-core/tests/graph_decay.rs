//! ADD task `graph-decay-recency` — GraphDecay type + default-port behavior
//! (contract FROZEN @ v1, 2026-06-11). No live backend needed.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::scope::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::capabilities::StorageCapabilities;
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphDecay, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};

#[test]
fn rejects_invalid_lambda() {
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5] {
        let err = GraphDecay::new(bad).expect_err("invalid λ must be rejected");
        assert!(
            err.to_string().contains("graph_decay_invalid_lambda"),
            "error must carry the named code, got: {err}"
        );
    }
    // Valid boundary + typical values construct and echo through accessors.
    let zero = GraphDecay::new(0.0).expect("λ=0 is valid (decay-neutral)");
    assert_eq!(zero.lambda(), 0.0);
    assert_eq!(zero.time_weight(), None);
    let typical = GraphDecay::new(2.5).expect("positive finite λ");
    assert_eq!(typical.lambda(), 2.5);
}

#[test]
fn rejects_invalid_time_weight() {
    let base = GraphDecay::new(1.0).expect("valid λ");
    for bad in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
        let err = base.with_time_weight(bad).expect_err("invalid w must be rejected");
        assert!(
            err.to_string().contains("graph_decay_invalid_time_weight"),
            "error must carry the named code, got: {err}"
        );
    }
    let weighted = base.with_time_weight(2.0).expect("positive finite w");
    assert_eq!(weighted.lambda(), 1.0);
    assert_eq!(weighted.time_weight(), Some(2.0));
}

/// Minimal port keeping the DEFAULT `graph_traverse_decayed`. Only
/// `graph_traverse` (marker result) and `capabilities` are reachable.
struct DefaultDecayPort;

#[async_trait]
impl StoragePort for DefaultDecayPort {
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
        Ok(GraphResult { headers: vec!["marker".into()], rows: vec![] })
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
/// `graph_decay_native` — at its non-Moon default.
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
async fn default_port_decay_some_not_supported() {
    let port = DefaultDecayPort;
    let scope = Scope::new("decay-default").unwrap();
    let q = CypherQuery {
        graph: "g".into(),
        cypher: "MATCH (n) RETURN n".into(),
        params: Default::default(),
    };
    let decay = GraphDecay::new(1.0).unwrap();
    let err = port
        .graph_traverse_decayed(&scope, &q, None, Some(&decay))
        .await
        .expect_err("default impl must refuse decay");
    assert!(matches!(err, StorageError::NotSupported(_)), "must be NotSupported, got {err:?}");
    assert!(err.to_string().contains("graph_decay_unsupported"), "named code, got: {err}");
}

#[tokio::test]
async fn default_port_decay_none_delegates() {
    let port = DefaultDecayPort;
    let scope = Scope::new("decay-default").unwrap();
    let q = CypherQuery {
        graph: "g".into(),
        cypher: "MATCH (n) RETURN n".into(),
        params: Default::default(),
    };
    let via_decayed = port.graph_traverse_decayed(&scope, &q, None, None).await.unwrap();
    let direct = port.graph_traverse(&scope, &q, None).await.unwrap();
    assert_eq!(via_decayed.headers, direct.headers, "None must delegate byte-for-byte");
    assert_eq!(via_decayed.headers, vec!["marker".to_string()]);
}

#[test]
fn capabilities_serde_default_is_false() {
    let caps = test_caps();
    let mut v = serde_json::to_value(caps).expect("serialize");
    v.as_object_mut().expect("object").remove("graph_decay_native");
    let back: StorageCapabilities = serde_json::from_value(v).expect("old payload must parse");
    assert!(!back.graph_decay_native, "missing field must default to false");
}
