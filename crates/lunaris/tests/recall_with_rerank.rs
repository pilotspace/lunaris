//! Plan 02-03 end-to-end smoke: ingest one Episode, recall via the DSL with
//! the `rerank(handle.reranker())` operator chained in, assert hit text + source
//! come back AND that `Hit { rerank_applied: false }` reflects the NoopReranker
//! the test wires (so we can prove the flag plumbing without needing the real
//! 1.1 GB bge-reranker-v2-m3 weights cached).
//!
//! Mirrors the Plan 02-02 `recall_smoke.rs` fixture (in-memory
//! RecordingStorageWithKeyword) so the test stays deterministic.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::{Lunaris, NoopReranker, Query, Reranker, Vector};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Episode, Hlc, HlcClock, StorageCapabilities, StorageError, StoragePort,
    StubEmbedder,
};
use parking_lot::Mutex;
use serde_json::json;

/// In-memory storage that records writes AND serves chunks back via
/// `read_as_of`. Mirrors the Phase 2 ingest pipeline keyspace.
#[derive(Default)]
struct RecordingStorageWithKeyword {
    rows: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    chunk_ids: Mutex<Vec<Vec<u8>>>,
}

impl RecordingStorageWithKeyword {
    fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StoragePort for RecordingStorageWithKeyword {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        for op in ops {
            match op {
                WriteOp::KvPut { key, value } => {
                    self.rows.lock().insert(key.clone(), value.clone());
                }
                WriteOp::VectorUpsert { id, .. } => {
                    self.chunk_ids.lock().push(id.clone());
                }
                _ => {}
            }
        }
        Ok(Lsn { wall_ms: 1, counter: 1 })
    }

    async fn vector_search(
        &self,
        _scope: &lunaris_core::Scope,
        _index: &str,
        _query: &[f32],
        k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        let ids = self.chunk_ids.lock().clone();
        Ok(ids
            .into_iter()
            .take(k)
            .enumerate()
            .map(|(i, id)| VectorHit {
                id,
                score: 1.0 - (i as f32 * 0.1),
                rerank_applied: false,
                metadata: json!({}),
            })
            .collect())
    }

    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("graph_traverse"))
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
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.lock().get(key).cloned().map(|v| Row {
            key: key.to_vec(),
            value: Bytes::from(v),
            bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
        }))
    }

    async fn publish(
        &self,
        _scope: &lunaris_core::Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("publish"))
    }

    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("subscribe"))
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
        }
    }
}

#[async_trait]
impl KeywordPort for RecordingStorageWithKeyword {
    async fn keyword_search(
        &self,
        _scope: &lunaris_core::Scope,
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
async fn recall_with_noop_reranker_returns_hits() {
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);

    let handle = Lunaris::with_parts_keyword(storage, keyword, embedder, clock.clone())
        .with_reranker(Arc::new(NoopReranker) as Arc<dyn Reranker>);

    // Ingest one Episode.
    let ep = Episode::new(
        lunaris_core::Scope::dev(),
        "notes.md",
        "# Notes\nThe quick brown fox jumps over the lazy dog.",
        &clock,
    );
    let _ = handle.ingest(ep).await.expect("ingest must succeed");

    // Recall via the DSL — vector → rerank(handle.reranker()) → top(3).
    let hits = handle
        .recall()
        .with_root(Vector::new("chunks", 30).rerank(handle.reranker()).top(3))
        .execute(Query::text("brown fox"))
        .await
        .expect("recall must succeed");

    assert!(!hits.is_empty(), "at least one hit must come back");
    // NoopReranker has applies()=false, so rerank_applied must be false on every hit.
    for h in &hits {
        assert!(
            !h.rerank_applied,
            "NoopReranker MUST flow `rerank_applied: false` into the Hit; got true"
        );
        assert!(!h.degraded, "no fallback path triggered; degraded must be false");
    }
    // First hit text MUST contain the search term — proves end-to-end hydration
    // through the rerank pass.
    let first = &hits[0];
    assert!(
        first.text.to_lowercase().contains("brown fox"),
        "first hit text must contain the search term: got `{}`",
        first.text
    );
    assert_eq!(first.source, "notes.md");
}

#[tokio::test]
async fn handle_reranker_accessor_returns_configured_reranker() {
    // The escape-hatch with_parts_keyword installs NoopReranker by default.
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);

    let handle = Lunaris::with_parts_keyword(storage, keyword, embedder, clock);
    // Default reranker is the NoopReranker — applies() must be false.
    assert!(
        !handle.reranker().applies(),
        "default reranker for with_parts_keyword MUST be a NoopReranker"
    );

    // After with_reranker(...) the new reranker takes over.
    struct AlwaysRealReranker;
    #[async_trait]
    impl Reranker for AlwaysRealReranker {
        async fn rerank(
            &self,
            _q: &str,
            docs: Vec<lunaris::RerankCandidate>,
        ) -> Result<Vec<lunaris::RerankCandidate>, lunaris::LunarisError> {
            Ok(docs)
        }
        fn applies(&self) -> bool {
            true
        }
    }
    let swapped = handle.with_reranker(Arc::new(AlwaysRealReranker) as Arc<dyn Reranker>);
    assert!(swapped.reranker().applies(), "with_reranker MUST install the new reranker");
}

#[tokio::test]
async fn rerank_then_degraded_fallback_composes() {
    // Compose: Vector → rerank(noop) wrapped in degraded_fallback(other Vector).
    // Primary path runs successfully (no error → fallback not triggered),
    // but proves the chain compiles end-to-end with both Plan 02-03 operators.
    let rec = Arc::new(RecordingStorageWithKeyword::new());
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);

    let handle = Lunaris::with_parts_keyword(storage, keyword, embedder, clock.clone())
        .with_reranker(Arc::new(NoopReranker) as Arc<dyn Reranker>);

    let ep = Episode::new(
        lunaris_core::Scope::dev(),
        "notes.md",
        "# Notes\nLazy dog and brown fox content here.",
        &clock,
    );
    let _ = handle.ingest(ep).await.expect("ingest must succeed");

    // Build: Vector("chunks", 30).rerank(noop).degraded_fallback(Vector("chunks", 5)).top(3)
    let root = Vector::new("chunks", 30)
        .rerank(handle.reranker())
        .degraded_fallback(Vector::new("chunks", 5))
        .top(3);

    let hits = handle.recall().with_root(root).execute(Query::text("brown fox")).await.unwrap();
    assert!(!hits.is_empty(), "compose must produce at least one hit");
    for h in &hits {
        assert!(!h.degraded, "primary succeeded — no degradation expected");
    }
}
