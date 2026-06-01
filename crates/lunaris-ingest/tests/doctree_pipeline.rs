//! DocTree pipeline integration tests — STRUCT-02.
//!
//! - `doctree_kvput_appears_in_ops`: proves the ingest pipeline includes exactly
//!   one DocTree KvPut per Episode in the atomic batch.
//! - `doctree_is_queryable_via_embedded_storage`: proves DocTree survives a full
//!   ingest → read_as_of round-trip via the zero-dep embedded SQLite backend.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::{
    CypherQuery, Episode, Filter, GraphResult, Hlc, HlcClock, Lsn, QueueMsg, Row, Scope,
    StorageCapabilities, StorageError, StoragePort, StubEmbedder, VectorHit, WriteOp,
    keyspace::doctree_key,
};
use lunaris_ingest::{DocTree, ingest_episode};
use lunaris_storage_embedded::EmbeddedStorage;
use parking_lot::Mutex;

// ---- RecordingStorage (minimal, reused from ingest_pipeline.rs) ----

#[derive(Default)]
struct RecordingStorage {
    batches: Mutex<Vec<Vec<WriteOp>>>,
}

impl RecordingStorage {
    fn first_batch(&self) -> Vec<WriteOp> {
        self.batches.lock().first().cloned().unwrap_or_default()
    }
}

#[async_trait]
impl StoragePort for RecordingStorage {
    async fn atomic_write(&self, _scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        self.batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: 1, counter: 0 })
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
        Err(StorageError::NotSupported("RecordingStorage::vector_search"))
    }

    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _query: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::graph_traverse"))
    }

    async fn scan_range(
        &self,
        _scope: &Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::scan_range"))
    }

    async fn read_as_of(
        &self,
        _scope: &Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::read_as_of"))
    }

    async fn publish(
        &self,
        _scope: &Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("RecordingStorage::publish"))
    }

    async fn subscribe(
        &self,
        _scope: &Scope,
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
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
        }
    }
}

// ---- helpers ----

fn heading_episode(clock: &HlcClock) -> Episode {
    let body = "# Introduction\n\nSome prose here.\n\n## Background\n\nMore details.\n\n# Conclusion\n\nFinal thoughts.\n";
    Episode::new(Scope::dev(), "doc.md", body, clock)
}

fn no_heading_episode(clock: &HlcClock) -> Episode {
    let body = "Just plain text with no headings at all. Some content here.\n";
    Episode::new(Scope::dev(), "flat.md", body, clock)
}

// ---- tests ----

/// Prove exactly one DocTree KvPut appears in the atomic batch.
#[tokio::test]
async fn doctree_kvput_appears_in_ops() {
    let storage = Arc::new(RecordingStorage::default());
    let embedder = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let ep = heading_episode(&clock);

    ingest_episode(&*storage, &*embedder, &clock, ep).await.expect("ingest ok");

    let batch = storage.first_batch();
    let n_doctree = batch
        .iter()
        .filter(
            |op| matches!(op, WriteOp::KvPut { key, .. } if key.windows(8).any(|w| w == b":doctree")),
        )
        .count();
    assert_eq!(n_doctree, 1, "exactly one DocTree KvPut must appear in the batch");
}

/// Prove DocTree is queryable via the embedded SQLite storage (memory://).
///
/// Full round-trip: ingest → atomic_write to embedded → read_as_of → deserialize DocTree.
#[tokio::test]
async fn doctree_is_queryable_via_embedded_storage() {
    let storage =
        EmbeddedStorage::connect("memory://").await.expect("embedded storage must connect");
    let embedder = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let ep = heading_episode(&clock);
    let episode_id = ep.id;
    let scope = ep.scope.clone();

    let lsn = ingest_episode(&storage, &*embedder, &clock, ep).await.expect("ingest ok");

    // Read back the DocTree using read_as_of at the commit HLC.
    let key = doctree_key(&scope, episode_id);
    // Use the HLC from the commit LSN to form a valid as_of timestamp.
    let as_of = Hlc { wall_ms: lsn.wall_ms, counter: lsn.counter, node_id: 0 };
    let row = storage.read_as_of(&scope, &key, as_of).await.expect("read_as_of must not error");

    let row = row.expect("DocTree row must exist after ingest");
    let tree: DocTree =
        serde_json::from_slice(&row.value).expect("DocTree must deserialize from stored bytes");

    // Must have at least one root (heading doc → H1 roots).
    assert!(!tree.roots.is_empty(), "DocTree must have at least one root");

    // The two H1 headings must appear as roots.
    let root_titles: Vec<&str> = tree.roots.iter().map(|r| r.title.as_str()).collect();
    assert!(
        root_titles.contains(&"Introduction"),
        "Introduction H1 must be a root; got: {root_titles:?}"
    );
    assert!(
        root_titles.contains(&"Conclusion"),
        "Conclusion H1 must be a root; got: {root_titles:?}"
    );

    // Background is a child of Introduction (H2 under H1).
    let intro = tree.roots.iter().find(|n| n.title == "Introduction").unwrap();
    let child_titles: Vec<&str> = intro.children.iter().map(|c| c.title.as_str()).collect();
    assert!(
        child_titles.contains(&"Background"),
        "Background H2 must be a child of Introduction; got: {child_titles:?}"
    );
}

/// No-heading document must produce a single-root DocTree that is queryable.
#[tokio::test]
async fn no_heading_doc_doctree_is_single_root_and_queryable() {
    let storage =
        EmbeddedStorage::connect("memory://").await.expect("embedded storage must connect");
    let embedder = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let ep = no_heading_episode(&clock);
    let episode_id = ep.id;
    let scope = ep.scope.clone();

    let lsn = ingest_episode(&storage, &*embedder, &clock, ep).await.expect("ingest ok");

    let key = doctree_key(&scope, episode_id);
    let as_of = Hlc { wall_ms: lsn.wall_ms, counter: lsn.counter, node_id: 0 };
    let row = storage
        .read_as_of(&scope, &key, as_of)
        .await
        .expect("read_as_of must not error")
        .expect("DocTree row must exist");

    let tree: DocTree = serde_json::from_slice(&row.value).expect("DocTree must deserialize");
    assert_eq!(tree.roots.len(), 1, "no-heading doc must have exactly one synthetic root");
    assert_eq!(tree.roots[0].level, 0, "synthetic root must have level=0");
    assert_eq!(tree.roots[0].title, "", "synthetic root must have empty title");
}
