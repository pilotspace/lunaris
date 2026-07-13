//! ADD task `hook-recall-graph-hybrid` (contract FROZEN @ v1.1, 2026-07-14):
//! behavioral shape pin for the hook's hybrid recall root.
//!
//! The v1.1 root is
//! `Vector("chunks",k).and(Keyword::bm25("chunks",k)).and(Vector("facts",k))
//!  .and(Keyword::bm25("facts",k)).fuse_rrf(60)`
//! built by `lunaris_hook::context::hybrid_root(k)` and executed under a
//! `QueryContext` whose `query_embedding` OnceCell is PRE-SEEDED from the
//! hook's embed cache — the placeholder embedder must provably never run.
//!
//! The operator structs' fields are `pub(crate)` to lunaris-retrieve, so the
//! shape is pinned BEHAVIORALLY (recorded port calls), not by downcasting —
//! §4: assert behavior, not internals.
//!
//! COMPILE-RED until `hybrid_root` lands — confined to this test binary.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    Embedder, Hlc, LunarisError, Scope, StorageCapabilities, StorageError, StoragePort,
};
use lunaris_hook::context::hybrid_root;
use lunaris_retrieve::{Query, QueryContext, Retriever};
use parking_lot::Mutex;
use serde_json::json;
use ulid::Ulid;

// ─── PanickingEmbedder — proves the pre-seeded OnceCell short-circuits ──────

struct PanickingEmbedder;

#[async_trait]
impl Embedder for PanickingEmbedder {
    fn dim(&self) -> usize {
        768
    }
    async fn embed_batch(&self, _inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        panic!("placeholder embedder invoked — the pre-seeded query_embedding must short-circuit");
    }
}

// ─── RecordingPort — records (index, k) per vector/keyword call ─────────────

#[derive(Default)]
struct RecordingPort {
    vector_calls: Mutex<Vec<(String, usize)>>,
    keyword_calls: Mutex<Vec<(String, usize)>>,
    /// ids returned per index so fusion provenance is checkable downstream.
    vector_ids: Mutex<std::collections::HashMap<String, Vec<Vec<u8>>>>,
    keyword_ids: Mutex<std::collections::HashMap<String, Vec<Vec<u8>>>>,
}

#[async_trait]
impl StoragePort for RecordingPort {
    async fn vector_search(
        &self,
        _scope: &Scope,
        index: &str,
        _query: &[f32],
        k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        self.vector_calls.lock().push((index.to_owned(), k));
        let ids = self.vector_ids.lock().get(index).cloned().unwrap_or_default();
        Ok(ids
            .into_iter()
            .enumerate()
            .map(|(rank, id)| VectorHit {
                id,
                score: 0.9 - rank as f32 * 0.1,
                rerank_applied: false,
                metadata: json!({}),
            })
            .collect())
    }

    async fn atomic_write(&self, _: &Scope, _: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
    async fn read_as_of(
        &self,
        _: &Scope,
        _: &[u8],
        _: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(None)
    }
    async fn graph_traverse(
        &self,
        _: &Scope,
        _: &CypherQuery,
        _: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("RecordingPort"))
    }
    async fn scan_range(
        &self,
        _: &Scope,
        _: &[u8],
        _: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(vec![]).boxed())
    }
    async fn publish(&self, _: &Scope, _: &str, _: u16, _: Bytes) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("RecordingPort"))
    }
    async fn subscribe(
        &self,
        _: &Scope,
        _: &str,
        _: &str,
        _: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("RecordingPort"))
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
            graph_navigate_native: false,
        }
    }
}

#[async_trait]
impl KeywordPort for RecordingPort {
    async fn keyword_search(
        &self,
        _scope: &Scope,
        index: &str,
        _query: &str,
        k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        self.keyword_calls.lock().push((index.to_owned(), k));
        let ids = self.keyword_ids.lock().get(index).cloned().unwrap_or_default();
        Ok(ids
            .into_iter()
            .enumerate()
            .map(|(rank, id)| {
                let score = 0.8 - rank as f32 * 0.1;
                KeywordHit { id, score, raw_score: score, metadata: json!({}) }
            })
            .collect())
    }
}

fn preseeded_ctx(rec: Arc<RecordingPort>) -> QueryContext {
    let storage: Arc<dyn StoragePort> = rec.clone();
    let keyword: Arc<dyn KeywordPort> = rec;
    let embedder: Arc<dyn Embedder> = Arc::new(PanickingEmbedder);
    let ctx = QueryContext::new(
        Query::text("which port does zephyr-relay listen on"),
        Scope::new("hybrid-root-shape").unwrap(),
        embedder,
        storage,
        keyword,
    );
    ctx.query_embedding.set(vec![0.1f32; 768]).expect("fresh OnceCell accepts the seed");
    ctx
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// §2 "hybrid root shape is the canonical fused tree" (v1.1): executing the
/// root issues EXACTLY vector_search on {chunks, facts} and keyword_search on
/// {chunks, facts}, once each, all at leg k — and the panicking placeholder
/// embedder never runs (the pre-seeded OnceCell short-circuits embed_once).
#[tokio::test]
async fn hybrid_root_queries_all_four_legs_without_embedding() {
    let rec = Arc::new(RecordingPort::default());
    let ctx = preseeded_ctx(rec.clone());

    let root = hybrid_root(7);
    let hits = root.retrieve(&ctx).await.expect("hybrid root retrieves on the mock port");

    let mut vector: Vec<(String, usize)> = rec.vector_calls.lock().clone();
    vector.sort();
    assert_eq!(
        vector,
        vec![("chunks".to_owned(), 7), ("facts".to_owned(), 7)],
        "vector legs: chunks + facts exactly once each at leg k"
    );
    let mut keyword: Vec<(String, usize)> = rec.keyword_calls.lock().clone();
    keyword.sort();
    assert_eq!(
        keyword,
        vec![("chunks".to_owned(), 7), ("facts".to_owned(), 7)],
        "keyword legs: chunks + facts exactly once each at leg k (amendment v1.1)"
    );
    // Empty port → empty fused output; the point of this test is the calls +
    // no-panic. Fusion provenance is asserted separately below.
    assert!(hits.is_empty());
}

/// Fusion spans the legs: a hit surfaced ONLY by the facts keyword leg and a
/// hit surfaced ONLY by the chunks vector leg BOTH appear in the fused output
/// with RRF-scale scores (Σ 1/(60+rank) — far below raw-cosine scale).
#[tokio::test]
async fn fused_output_carries_vector_only_and_keyword_only_hits() {
    let rec = Arc::new(RecordingPort::default());
    let chunk_id = Ulid::new().to_bytes().to_vec();
    let fact_id = Ulid::new().to_bytes().to_vec();
    rec.vector_ids.lock().insert("chunks".to_owned(), vec![chunk_id.clone()]);
    rec.keyword_ids.lock().insert("facts".to_owned(), vec![fact_id.clone()]);
    let ctx = preseeded_ctx(rec);

    let root = hybrid_root(5);
    let hits = root.retrieve(&ctx).await.expect("hybrid root retrieves");

    let ids: Vec<&Vec<u8>> = hits.iter().map(|h| &h.id).collect();
    assert!(ids.contains(&&chunk_id), "vector-only chunk hit fused in");
    assert!(ids.contains(&&fact_id), "keyword-only fact hit fused in (facts BM25 leg)");
    for hit in &hits {
        assert!(
            hit.score < 0.2,
            "fused scores are RRF-scale (Σ 1/(60+rank)), got {} — raw scores would \
             annihilate under the cosine min_score threshold",
            hit.score
        );
    }
}
