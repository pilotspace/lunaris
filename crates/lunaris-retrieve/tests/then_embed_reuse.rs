//! Plan 260610-f91: ThenRetriever must reuse the parent QueryContext's computed
//! embedding instead of causing a second embedder forward pass.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

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
use lunaris_retrieve::{Query, QueryContext, RawHit, Retriever, SourceOp};
use serde_json::json;

// ─── CountingEmbedder ────────────────────────────────────────────────────────

struct CountingEmbedder {
    count: Arc<AtomicUsize>,
    inner: StubEmbedder,
}

impl CountingEmbedder {
    fn new(dim: usize) -> (Self, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        (Self { count: count.clone(), inner: StubEmbedder::new(dim) }, count)
    }
}

#[async_trait]
impl Embedder for CountingEmbedder {
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.inner.embed_batch(texts).await
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
}

// ─── EmptyStorage ────────────────────────────────────────────────────────────

struct EmptyStorage;

#[async_trait]
impl StoragePort for EmptyStorage {
    async fn atomic_write(&self, _: &Scope, _: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
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
        Ok(vec![])
    }
    async fn graph_traverse(
        &self,
        _: &Scope,
        _: &CypherQuery,
        _: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("EmptyStorage"))
    }
    async fn scan_range(
        &self,
        _: &Scope,
        _: &[u8],
        _: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(vec![]).boxed())
    }
    async fn read_as_of(
        &self,
        _: &Scope,
        _: &[u8],
        _: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(None)
    }
    async fn publish(&self, _: &Scope, _: &str, _: u16, _: Bytes) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("EmptyStorage"))
    }
    async fn subscribe(
        &self,
        _: &Scope,
        _: &str,
        _: &str,
        _: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("EmptyStorage"))
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

#[async_trait]
impl KeywordPort for EmptyStorage {
    async fn keyword_search(
        &self,
        _: &Scope,
        _: &str,
        _: &str,
        _: usize,
        _: Option<&Filter>,
        _: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(vec![])
    }
}

// ─── EmbeddingProbRetriever ───────────────────────────────────────────────────
// A Retriever that calls ctx.embed_once() so we can count embedder calls.

struct OneHitRetriever(Vec<u8>);

#[async_trait]
impl Retriever for OneHitRetriever {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        // Force embed_once so the OnceCell is populated (or reused).
        let _ = ctx.embed_once().await?;
        Ok(vec![RawHit {
            id: self.0.clone(),
            score: 1.0,
            rerank_applied: false,
            degraded: false,
            metadata: json!({}),
            source_op: SourceOp::Vector,
        }])
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// ThenRetriever: first leg embeds query, second leg MUST reuse it (count = 1).
/// Before fix: narrowed ctx has empty OnceCell → embed_batch fires again → count = 2.
/// After fix: narrowed ctx is pre-seeded → OnceCell already initialized → count = 1.
#[tokio::test]
async fn then_retriever_reuses_parent_embedding() {
    let (embedder, count) = CountingEmbedder::new(768);
    let storage: Arc<dyn StoragePort> = Arc::new(EmptyStorage);
    let keyword: Arc<dyn KeywordPort> = Arc::new(EmptyStorage);
    let embedder: Arc<dyn Embedder> = Arc::new(embedder);

    let ctx =
        QueryContext::new(Query::text("what is memory"), Scope::dev(), embedder, storage, keyword);

    let fake_id = vec![0u8; 16];
    let then_ret = lunaris_retrieve::then(
        Box::new(OneHitRetriever(fake_id.clone())),
        Box::new(OneHitRetriever(fake_id)),
    );

    let _ = then_ret.retrieve(&ctx).await.unwrap();

    let embed_calls = count.load(Ordering::SeqCst);
    assert_eq!(
        embed_calls, 1,
        "ThenRetriever MUST reuse parent embedding — expected 1 embedder call, got {embed_calls}"
    );
}

/// ThenRetriever: cold path (parent ctx has no embedding yet) → count = 1.
/// The first leg triggers embed_once; the seeded narrowed ctx sees it already set.
#[tokio::test]
async fn then_retriever_cold_path_embeds_once() {
    let (embedder, count) = CountingEmbedder::new(768);
    let storage: Arc<dyn StoragePort> = Arc::new(EmptyStorage);
    let keyword: Arc<dyn KeywordPort> = Arc::new(EmptyStorage);
    let embedder: Arc<dyn Embedder> = Arc::new(embedder);

    let ctx =
        QueryContext::new(Query::text("test query"), Scope::dev(), embedder, storage, keyword);
    // Verify: parent ctx has no pre-loaded embedding.
    assert!(ctx.query_embedding.get().is_none());

    let fake_id = vec![1u8; 16];
    let then_ret = lunaris_retrieve::then(
        Box::new(OneHitRetriever(fake_id.clone())),
        Box::new(OneHitRetriever(fake_id)),
    );

    let _ = then_ret.retrieve(&ctx).await.unwrap();

    let embed_calls = count.load(Ordering::SeqCst);
    assert_eq!(embed_calls, 1, "cold ThenRetriever MUST embed exactly once; got {embed_calls}");
}
