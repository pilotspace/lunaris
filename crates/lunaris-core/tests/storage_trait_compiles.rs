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
    async fn atomic_write(&self, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Err(StorageError::NotSupported("stub"))
    }
    async fn vector_search(
        &self,
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
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("stub"))
    }
    async fn scan_range(
        &self,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Err(StorageError::NotSupported("stub"))
    }
    async fn read_as_of(&self, _k: &[u8], _as_of: Hlc) -> Result<Option<Row<Bytes>>, StorageError> {
        Err(StorageError::NotSupported("stub"))
    }
    async fn publish(&self, _t: &str, _p: u16, _payload: Bytes) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("stub"))
    }
    async fn subscribe(
        &self,
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
    let r = s.atomic_write(&[]).await;
    assert!(matches!(r, Err(StorageError::NotSupported(_))));
    let cap = s.capabilities();
    assert!(cap.bi_temporal_native);
    assert_eq!(cap.max_vector_dim, 768);
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
