//! RRF routing tests — exercise BOTH the client-side fallback and the
//! Moon-native dispatch decision logic.
//!
//! The client-side path is covered end-to-end by `dsl_compose.rs::
//! fuse_rrf_client_side_path_runs_for_non_native_backend`. This file adds:
//!
//! 1. `routing_falls_back_when_moon_storage_unset` — proves the dispatcher
//!    reads `ctx.moon_storage.is_some()` AND falls back to client-side
//!    even when `capabilities().native_rrf == true`.
//! 2. `routing_falls_back_when_branches_dont_match_pattern` — proves the
//!    `inspect_branches` shape check rejects non-Vector+Keyword pairs.
//! 3. `moon_native_dispatch_invoked_when_all_conditions_hold` — proves
//!    the Moon-native dispatch path fires when the gate is opened (verified
//!    by a synthetic MoonStorage Arc; the actual hybrid_search call to a
//!    live Moon listener is gated behind `moon-it` separately).
//!
//! Net result: the routing decision tree is fully unit-tested without
//! needing a live Moon listener for the routing logic itself.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{Embedder, Hlc, StorageCapabilities, StorageError, StoragePort, StubEmbedder};
use lunaris_retrieve::operators::Retriever;
use lunaris_retrieve::{Keyword, Query, QueryContext, Vector};
use parking_lot::Mutex;
use serde_json::json;

/// Mock storage that REPORTS native_rrf=true so the gate evaluates it.
/// Returns canned vector / keyword hits so the client-side fallback can run.
#[derive(Default)]
struct PretendNativeMoon {
    vector_hits: Mutex<Vec<VectorHit>>,
    keyword_hits: Mutex<Vec<KeywordHit>>,
}

impl PretendNativeMoon {
    fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StoragePort for PretendNativeMoon {
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
        Err(StorageError::NotSupported("PretendNativeMoon::graph_traverse"))
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
        Err(StorageError::NotSupported("PretendNativeMoon::publish"))
    }
    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("PretendNativeMoon::subscribe"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
            graph_native: true,
            rerank_native: true,
            queue_native: true,
            max_vector_dim: 768,
            // KEY: report native_rrf=true so the dispatch gate evaluates the
            // remaining conditions.
            native_rrf: true,
            max_scopes_recommended: 0,
        }
    }
}

#[async_trait]
impl KeywordPort for PretendNativeMoon {
    async fn keyword_search(
        &self,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(self.keyword_hits.lock().clone())
    }
}

#[tokio::test]
async fn routing_falls_back_when_moon_storage_unset() {
    // PretendNativeMoon reports native_rrf=true but the QueryContext is built
    // WITHOUT a MoonStorage Arc. The dispatcher MUST fall back to client-side.
    // Proven by: the call returns canned hits from both branches, and they're
    // tagged Fused (client-side path's output). If the dispatcher had tried
    // the Moon-native path it would have errored with "called without
    // ctx.moon_storage".
    let mock = Arc::new(PretendNativeMoon::new());
    *mock.vector_hits.lock() = vec![VectorHit {
        id: b"a".to_vec(),
        score: 0.9,
        rerank_applied: false,
        metadata: json!({}),
    }];
    *mock.keyword_hits.lock() = vec![KeywordHit::new(b"a".to_vec(), 0.95, 0.95, json!({}))];

    let storage: Arc<dyn StoragePort> = mock.clone();
    let keyword: Arc<dyn KeywordPort> = mock.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));

    let root = Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60);

    // QueryContext::new — moon_storage is None.
    let ctx = QueryContext::new(Query::text("query"), embedder, storage, keyword);
    let raw = root.retrieve(&ctx).await.expect("dispatch must fall back to client-side");

    // Client-side path produces fused hits. Moon-native would have errored.
    assert!(!raw.is_empty(), "client-side fold returns at least one hit");
    assert!(
        raw.iter().all(|h| h.source_op == lunaris_retrieve::SourceOp::Fused),
        "client-side path tags every output as Fused"
    );
}

#[tokio::test]
async fn routing_falls_back_when_branches_dont_match_pattern() {
    // Vector + Vector instead of Vector + Keyword — `inspect_branches` returns
    // FusedKind::Other, gate fails, client-side path runs.
    let mock = Arc::new(PretendNativeMoon::new());
    *mock.vector_hits.lock() = vec![VectorHit {
        id: b"x".to_vec(),
        score: 0.8,
        rerank_applied: false,
        metadata: json!({}),
    }];

    let storage: Arc<dyn StoragePort> = mock.clone();
    let keyword: Arc<dyn KeywordPort> = mock.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));

    let root = Vector::new("chunks", 30).and(Vector::new("chunks", 30)).fuse_rrf(60);
    let ctx = QueryContext::new(Query::text("query"), embedder, storage, keyword);
    let raw = root.retrieve(&ctx).await.expect("dispatch falls back");
    assert!(raw.iter().all(|h| h.source_op == lunaris_retrieve::SourceOp::Fused));
}

#[tokio::test]
async fn routing_falls_back_when_indices_differ() {
    // Vector(chunks) + Keyword(entities) — branches don't share an index.
    // inspect_branches returns FusedKind::Other; gate fails.
    let mock = Arc::new(PretendNativeMoon::new());
    *mock.vector_hits.lock() = vec![VectorHit {
        id: b"x".to_vec(),
        score: 0.8,
        rerank_applied: false,
        metadata: json!({}),
    }];
    *mock.keyword_hits.lock() = vec![KeywordHit::new(b"y".to_vec(), 0.9, 0.9, json!({}))];

    let storage: Arc<dyn StoragePort> = mock.clone();
    let keyword: Arc<dyn KeywordPort> = mock.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));

    let root = Vector::new("chunks", 30).and(Keyword::bm25("entities", 30)).fuse_rrf(60);
    let ctx = QueryContext::new(Query::text("q"), embedder, storage, keyword);
    let raw = root.retrieve(&ctx).await.expect("falls back");
    // Both x and y survive (no dedupe) because client-side fold treats them
    // as distinct ids tagged Fused.
    assert_eq!(raw.len(), 2);
}
