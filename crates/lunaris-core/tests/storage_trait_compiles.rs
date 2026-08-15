//! Compile-time + smoke checks that `StoragePort` is dyn-compatible and that supporting
//! types serialize correctly.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::*;

struct StubStorage;

#[async_trait]
impl StoragePort for StubStorage {
    async fn atomic_write(&self, _scope: &Scope, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Err(StorageError::NotSupported("stub"))
    }
    async fn vector_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _q: &[f32],
        _k: usize,
        _f: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Err(StorageError::NotSupported("stub"))
    }
    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("stub"))
    }
    async fn scan_range(
        &self,
        _scope: &Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Err(StorageError::NotSupported("stub"))
    }
    async fn read_as_of(
        &self,
        _scope: &Scope,
        _k: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Err(StorageError::NotSupported("stub"))
    }
    async fn publish(
        &self,
        _scope: &Scope,
        _t: &str,
        _p: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("stub"))
    }
    async fn subscribe(
        &self,
        _scope: &Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("stub"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
            graph_native: true,
            rerank_native: false,
            queue_native: true,
            max_vector_dim: 768,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn arc_dyn_storage_port_is_send_sync() {
    assert_send_sync::<Arc<dyn StoragePort>>();
    let _: Arc<dyn StoragePort> = Arc::new(StubStorage);
}

#[tokio::test]
async fn stub_returns_not_supported() {
    let s: Arc<dyn StoragePort> = Arc::new(StubStorage);
    let r = s.atomic_write(&Scope::new("test-scope").unwrap(), &[]).await;
    assert!(matches!(r, Err(StorageError::NotSupported(_))));
    let cap = s.capabilities();
    assert!(cap.bi_temporal_native);
    assert_eq!(cap.max_vector_dim, 768);
}

/// `list_scopes` is additive: backends that do not override the trait default
/// must compile unchanged AND return `Err(StorageError::NotSupported(_))` so
/// higher layers (e.g. Helios `memories.search`) can degrade to a
/// caller-supplied scope list.
#[tokio::test]
async fn list_scopes_default_impl_returns_not_supported() {
    let s: Arc<dyn StoragePort> = Arc::new(StubStorage);
    let r = s.list_scopes(None, 10, None).await;
    match r {
        Err(StorageError::NotSupported(msg)) => {
            assert!(
                msg.contains("list_scopes"),
                "NotSupported message should mention list_scopes, got: {msg}"
            );
        }
        other => panic!("expected NotSupported, got {other:?}"),
    }
}

/// `ScopePage` is part of the public surface; it must be `Default` (empty
/// page is valid) and round-trip through serde so RPC + persistence layers
/// can carry it.
#[test]
fn scope_page_default_and_serde_roundtrip() {
    use lunaris_core::ScopePage;
    let empty = ScopePage::default();
    assert!(empty.scopes.is_empty());
    assert!(empty.next_cursor.is_none());

    let page = ScopePage {
        scopes: vec![Scope::new("a").unwrap(), Scope::new("b").unwrap()],
        next_cursor: Some("opaque-cursor".to_string()),
    };
    let s = serde_json::to_string(&page).expect("serialize ScopePage");
    let back: ScopePage = serde_json::from_str(&s).expect("deserialize ScopePage");
    assert_eq!(back, page);
}

#[test]
fn write_op_variants_roundtrip() {
    let ops = vec![
        WriteOp::KvPut { key: b"k".to_vec(), value: b"v".to_vec() },
        WriteOp::KvDelete { key: b"k".to_vec() },
        WriteOp::VectorUpsert {
            index: "chunk_emb".into(),
            id: b"id1".to_vec(),
            embedding: vec![0.1, 0.2],
            metadata: serde_json::json!({"src":"notes.md"}),
        },
        WriteOp::GraphNode {
            graph: "lunaris_graph".into(),
            id: b"n1".to_vec(),
            label: "Person".into(),
            props: serde_json::json!({"name":"Alice"}),
            index_kind: "entities".into(),
        },
        WriteOp::GraphEdge {
            graph: "lunaris_graph".into(),
            src: b"n1".to_vec(),
            dst: b"n2".to_vec(),
            rel: "WORKS_AT".into(),
            props: serde_json::json!({"since":"2024-04"}),
        },
    ];
    for op in &ops {
        let s = serde_json::to_string(op).unwrap();
        let back: WriteOp = serde_json::from_str(&s).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), s);
    }
}

#[test]
fn filter_nested_roundtrip() {
    let f = Filter::And(vec![
        Filter::Eq { field: "source".into(), value: serde_json::Value::String("notes.md".into()) },
        Filter::StartsWith { field: "source".into(), prefix: "helios:fs/".into() },
    ]);
    let s = serde_json::to_string(&f).unwrap();
    let back: Filter = serde_json::from_str(&s).unwrap();
    assert_eq!(serde_json::to_string(&back).unwrap(), s);
}

#[test]
fn lsn_from_hlc_preserves_components() {
    let c = HlcClock::new(0);
    let t = c.tick();
    let l = Lsn::from_hlc(t);
    assert_eq!(l.wall_ms, t.wall_ms);
    assert_eq!(l.counter, t.counter);
}
