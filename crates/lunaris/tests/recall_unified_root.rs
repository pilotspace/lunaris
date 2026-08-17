//! GA-1 — `Lunaris::recall()` composes THE unified production root, and the
//! opt-in cross-encoder rerank stage is gated on `LUNARIS_RECALL_RERANK`
//! (read once at handle construction, default OFF).
//!
//! Surface-conformance pins (drift-class guard): the default builder root
//! must be byte-identical (by `plan_repr`) to
//! `lunaris_retrieve::production_root(30, graph_enabled)` — a future
//! `with_root` divergence on the HTTP/SDK surface fails a NAMED test here.
//!
//! Rerank gating pins:
//! - OFF (default) → the reranker Arc is NEVER consulted (no `applies()`, no
//!   `rerank()` — proven with a panicking reranker; mirrors the
//!   `lazy_reranker_rss.rs` "OFF must not load the model" contract without
//!   needing the 446 MiB GGUF).
//! - ON → exactly one rerank pass runs between fusion and the final top-k.
//!
//! RED until `production_root` / `plan_repr` / `RetrievalBuilder::root_plan`
//! / `RecallRerankConfig` + `Lunaris::with_recall_rerank` land (compile-red
//! confined to this test binary).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::{Lunaris, Query, RecallRerankConfig};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    Embedder, Hlc, HlcClock, LunarisError, Scope, StorageCapabilities, StorageError, StoragePort,
    StubEmbedder,
};
use lunaris_rerank::{RerankCandidate, Reranker};
use lunaris_retrieve::{plan_repr, production_root, production_root_reranked};
use ulid::Ulid;

// ─── Minimal storage stub — one canned chunk-index vector hit ───────────────

#[derive(Default)]
struct OneHitStorage;

#[async_trait]
impl StoragePort for OneHitStorage {
    async fn atomic_write(&self, _scope: &Scope, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
    async fn vector_search(
        &self,
        _scope: &Scope,
        index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        if index == "chunks" {
            Ok(vec![VectorHit {
                id: Ulid::new().to_bytes().to_vec(),
                score: 0.9,
                rerank_applied: false,
                metadata: serde_json::json!({}),
            }])
        } else {
            Ok(Vec::new())
        }
    }
    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }
    async fn scan_range(
        &self,
        _scope: &Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new()).boxed())
    }
    async fn read_as_of(
        &self,
        _scope: &Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(None)
    }
    async fn publish(
        &self,
        _s: &Scope,
        _t: &str,
        _p: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("OneHitStorage::publish"))
    }
    async fn subscribe(
        &self,
        _s: &Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("OneHitStorage::subscribe"))
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
impl KeywordPort for OneHitStorage {
    async fn keyword_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(Vec::new())
    }
}

// ─── Reranker doubles ───────────────────────────────────────────────────────

/// Panics on ANY consultation — proves the OFF path never touches the
/// reranker (the lazy-GGUF load can only fire from inside these methods).
struct PanickingReranker;

#[async_trait]
impl Reranker for PanickingReranker {
    async fn rerank(
        &self,
        _query: &str,
        _docs: Vec<RerankCandidate>,
    ) -> Result<Vec<RerankCandidate>, LunarisError> {
        panic!(
            "rerank() invoked with LUNARIS_RECALL_RERANK off — the OFF path must never touch the reranker"
        );
    }
    fn applies(&self) -> bool {
        panic!(
            "applies() invoked with LUNARIS_RECALL_RERANK off — the OFF path must never touch the reranker"
        );
    }
}

/// Counts rerank passes; passthrough scores.
#[derive(Default)]
struct CountingReranker {
    calls: AtomicUsize,
}

#[async_trait]
impl Reranker for CountingReranker {
    async fn rerank(
        &self,
        _query: &str,
        docs: Vec<RerankCandidate>,
    ) -> Result<Vec<RerankCandidate>, LunarisError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(docs)
    }
    fn applies(&self) -> bool {
        true
    }
}

fn handle() -> Lunaris {
    let storage = Arc::new(OneHitStorage);
    Lunaris::with_parts_keyword(
        storage.clone() as Arc<dyn StoragePort>,
        storage as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder::new(768)) as Arc<dyn Embedder>,
        HlcClock::new(0),
    )
}

// ─── Surface conformance: default root IS production_root ───────────────────

#[test]
fn default_root_matches_production_root_graph_off() {
    let h = handle();
    assert!(!h.graph_pipeline().is_enabled());
    assert_eq!(
        h.recall().root_plan(),
        plan_repr(&production_root(30, false)),
        "Lunaris::recall() graph-OFF default root must be production_root(30, false)"
    );
}

#[test]
fn default_root_matches_production_root_graph_on() {
    let h = handle();
    h.graph_pipeline().enable();
    assert_eq!(
        h.recall().root_plan(),
        plan_repr(&production_root(30, true)),
        "Lunaris::recall() graph-ON default root must be production_root(30, true)"
    );
}

// ─── Rerank gating ──────────────────────────────────────────────────────────

#[test]
fn rerank_on_adds_the_stage_to_the_default_root() {
    let reranker: Arc<dyn Reranker> = Arc::new(CountingReranker::default());
    let h = handle()
        .with_reranker(reranker.clone())
        .with_recall_rerank(RecallRerankConfig { enabled: true, top_in: None });
    assert_eq!(
        h.recall().root_plan(),
        plan_repr(&production_root_reranked(30, false, reranker, None)),
        "rerank-ON default root must be production_root_reranked(30, graph, reranker, None)"
    );
}

#[test]
fn rerank_off_root_carries_no_rerank_stage() {
    let h = handle();
    assert!(!h.recall_rerank().enabled, "rerank must default OFF");
    assert!(
        !h.recall().root_plan().contains("rerank("),
        "rerank-OFF default root must not contain a rerank stage; got {}",
        h.recall().root_plan()
    );
}

#[tokio::test]
async fn rerank_off_never_touches_the_reranker() {
    let h = handle().with_reranker(Arc::new(PanickingReranker));
    let scope = Scope::new("ga1-rerank-off").unwrap();
    // Executes the full default pipeline over a real hit — any consultation
    // of the reranker (applies() OR rerank()) panics the test.
    let hits = h.scoped(scope).recall(Query::text("anything")).await.expect("recall must succeed");
    // The canned hit has no KV row, so hydration drops it — the point is the
    // pipeline RAN without touching the reranker.
    assert!(hits.is_empty());
}

#[tokio::test]
async fn rerank_on_invokes_exactly_one_rerank_pass() {
    let counting = Arc::new(CountingReranker::default());
    let h = handle()
        .with_reranker(counting.clone() as Arc<dyn Reranker>)
        .with_recall_rerank(RecallRerankConfig { enabled: true, top_in: None });
    let scope = Scope::new("ga1-rerank-on").unwrap();
    let _ = h.scoped(scope).recall(Query::text("anything")).await.expect("recall must succeed");
    assert_eq!(
        counting.calls.load(Ordering::SeqCst),
        1,
        "rerank-ON recall must run exactly one cross-encoder pass"
    );
}

// ─── Env semantics (pure fn — no process-env mutation, edition 2024) ────────

#[test]
fn rerank_env_truthy_set_matches_graph_toggle_semantics() {
    for on in ["1", "true", "TRUE", "on", "ON"] {
        assert!(
            RecallRerankConfig::from_values(Some(on), None).enabled,
            "{on:?} must enable rerank"
        );
    }
    for off in ["0", "false", "FALSE", "off", "OFF", "yes", "2", ""] {
        assert!(
            !RecallRerankConfig::from_values(Some(off), None).enabled,
            "{off:?} must NOT enable rerank"
        );
    }
    assert!(!RecallRerankConfig::from_values(None, None).enabled, "unset must default OFF");
}

#[test]
fn rerank_top_in_env_parses_positive_integers_only() {
    assert_eq!(RecallRerankConfig::from_values(Some("1"), Some("40")).top_in, Some(40));
    assert_eq!(RecallRerankConfig::from_values(Some("1"), Some("0")).top_in, None);
    assert_eq!(RecallRerankConfig::from_values(Some("1"), Some("nope")).top_in, None);
    assert_eq!(RecallRerankConfig::from_values(Some("1"), None).top_in, None);
}
