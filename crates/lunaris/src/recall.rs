//! `Lunaris::recall` — the public Phase 2 retrieve entry point.
//!
//! Returns a [`RetrievalBuilder`] seeded with this handle's storage,
//! keyword, embedder, and (when present) the typed `Arc<MoonStorage>`
//! that lets `fuse_rrf` take the Moon-native one-round-trip path.
//!
//! ## Plan 04-04 B-8 — sync `recall()` is unchanged
//!
//! `recall()` stays synchronous so all prior callers (Phase 2 tests,
//! `recall_smoke.rs`, `graph_pipeline_smoke.rs`) keep compiling without
//! modification. The new async [`Lunaris::recall_with_degraded_check`] is
//! the variant that reads the verifier-queue depth and seeds the builder
//! with `initial_degraded` (VERIFY-05 + VERIFY-06).

use lunaris_core::LunarisError;
use lunaris_retrieve::RetrievalBuilder;
// Plan 05-05 OPS-05 — `Instrument::instrument` wraps the per-call body in the
// `lunaris.recall` info_span so per-call `correlation_id` field-recording
// + downstream child-span propagation works (CONTEXT.md D-24).
use tracing::Instrument;

use crate::handle::Lunaris;

/// Env knob for the verifier-queue-lag warning threshold (D-12). Recall
/// returns `degraded=true` when `queue_depth > threshold` at recall start.
const ENV_VERIFY_WARN_THRESHOLD: &str = "LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD";

/// Default verifier-queue warning threshold. Tuned conservatively for v0 — a
/// production deployment with a real backlog of 1000+ unverified items
/// should already be visible to ops and the recall API consumer.
const DEFAULT_VERIFY_WARN_THRESHOLD: u64 = 1000;

/// Verify queue topic — must match
/// [`lunaris_verify::worker::VERIFY_TOPIC`] verbatim.
const VERIFY_TOPIC: &str = "__lunaris_verify__";

/// GA-1: default `k` for the unified production recall root composed by
/// [`Lunaris::recall`]. Matches the pre-GA-1 `Vector::new("chunks", 30)`
/// candidate width and blueprint §4.2's top-30 sizing.
const DEFAULT_RECALL_K: usize = 30;

impl Lunaris {
    /// Build a [`RetrievalBuilder`] bound to this handle's storage, keyword,
    /// embedder, and (when available) the typed `Arc<MoonStorage>` that
    /// enables the Phase 1.5 RRF Moon-native dispatch.
    ///
    /// Default root operator is the GA-1 unified production root
    /// (`lunaris_retrieve::production_root(30, graph_enabled)`):
    /// `Vector ∧ BM25("chunks") → fuse_rrf(60) → top(30)`, plus the fact
    /// legs when the graph pipeline is ON, plus the opt-in
    /// `LUNARIS_RECALL_RERANK` cross-encoder stage. Callers replace
    /// the root via [`RetrievalBuilder::with_root`] for the canonical example
    /// from blueprint §8:
    ///
    /// ```no_run
    /// use lunaris::{Keyword, Lunaris, LunarisError, Query, Vector};
    ///
    /// # async fn demo(engine: Lunaris) -> Result<(), LunarisError> {
    /// let hits = engine.recall()
    ///     .with_root(Vector::new("chunks", 30)
    ///         .and(Keyword::bm25("chunks", 30))
    ///         .fuse_rrf(60)
    ///         .top(5))
    ///     .filter_str("source LIKE 'helios:fs/%'").unwrap()
    ///     .execute(Query::text("brown fox"))
    ///     .await?;
    /// # Ok(()) }
    /// ```
    ///
    /// Plan 03-03 graph-aware extension — once `handle.graph_pipeline().enable()`
    /// is called and Episodes have been ingested with extracted Entities,
    /// callers can compose `Graph::anchored` into the same DSL:
    ///
    /// ```no_run
    /// use lunaris::{EntityId, Graph, Lunaris, LunarisError, Query, Vector};
    ///
    /// # async fn demo(engine: Lunaris) -> Result<(), LunarisError> {
    /// let alice = EntityId::from_name_and_type("Alice", "Person");
    /// let hits = engine.recall()
    ///     .with_root(Vector::new("chunks", 30)
    ///         .and(Graph::anchored(vec![(alice, 1.0)], 2))
    ///         .fuse_rrf(60)
    ///         .rerank(engine.reranker())
    ///         .top(5))
    ///     .execute(Query::text("Tell me about Alice"))
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn recall(&self) -> RetrievalBuilder {
        // Wave 2.5C: bare Lunaris::recall() seeds Scope::dev() for backwards
        // compat. Adopters should migrate to engine.scoped(scope).recall() so
        // retrieval is scope-isolated at the storage layer. The warn fires once
        // per call so it's visible in tracing output without log-spamming.
        // scope-dev-allowed: bare-recall-fallback — Lunaris::recall warns and
        // degrades; ScopedLunaris::recall is canonical.
        tracing::warn!(
            "Lunaris::recall() uses Scope::dev() — migrate to engine.scoped(scope).recall() for scope-isolated retrieval"
        );
        let mut b = RetrievalBuilder::from_handle(self.storage(), self.keyword(), self.embedder());
        if let Some(moon) = self.moon_storage() {
            b = b.with_moon_storage(moon);
        }
        // Phase 14.2: thread the per-handle boost cache into every builder so
        // the post-hydrate rescorer can apply boost deltas.  The Arc clone is
        // cheap — the underlying LruCache is shared across all RetrievalBuilders
        // spawned from this handle.
        b = b.with_boost_cache(self.boost_cache.clone());
        // GA-1 (2026-08-17): the default root is THE unified production root
        // (`lunaris_retrieve::production_root`) — one composition, every
        // surface (MCP memory.recall, hook, HTTP /v1/recall, SDK). Graph-OFF
        // callers now get `Vector ∧ BM25("chunks") → fuse_rrf(60) → top(30)`
        // (a DELIBERATE default change: pre-GA-1 this was a bare
        // Vector("chunks",30)); graph-ON adds the fact legs (the hook-proven
        // `hybrid_root` structure, KG-RAG Wave B). The opt-in cross-encoder
        // stage (`LUNARIS_RECALL_RERANK`, frozen at construction) composes
        // between fusion and the final top-k; when OFF the reranker Arc is
        // never touched, so the lazy GGUF never loads. Callers can still
        // override via `.with_root(...)` as before. Conformance pin:
        // tests/recall_unified_root.rs.
        let graph_on = self.graph_pipeline().is_enabled();
        let rerank = self.recall_rerank();
        if rerank.enabled {
            b = b.with_root(lunaris_retrieve::production_root_reranked(
                DEFAULT_RECALL_K,
                graph_on,
                self.reranker(),
                rerank.top_in,
            ));
        } else {
            b = b.with_root(lunaris_retrieve::production_root(DEFAULT_RECALL_K, graph_on));
        }
        b
    }

    /// Plan 04-04 B-8 fix: NEW async variant that reads the verifier queue
    /// depth ONCE per call and seeds the resulting [`RetrievalBuilder`] with
    /// `with_initial_degraded(true)` when the depth crosses
    /// `LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` (default 1000).
    ///
    /// The existing sync [`Self::recall`] is unchanged — this is purely an
    /// additive method so prior callers (Phase 2 tests, recall_smoke.rs,
    /// graph_pipeline_smoke.rs) keep compiling without modification.
    ///
    /// Closes **VERIFY-05** (queue lag observability) + **VERIFY-06**
    /// (backpressure surfaces in recall responses as `Hit::degraded`).
    ///
    /// When the underlying [`lunaris_core::StoragePort::queue_depth`] returns
    /// `Err(StorageError::NotSupported(_))` (older backends without the
    /// additive method implemented), the call falls through with
    /// `initial_degraded=false` rather than failing the recall — the queue
    /// introspection is best-effort observability, not a hard correctness
    /// requirement.
    pub async fn recall_with_degraded_check(&self) -> Result<RetrievalBuilder, LunarisError> {
        // Plan 05-05 OPS-05 — `lunaris.recall` root span (CONTEXT.md D-24).
        // `correlation_id` is reserved as `tracing::field::Empty` so the
        // HTTP middleware (Plan 05-05 Task 3 `lunaris-server::middleware::tracing`)
        // can `Span::current().record("correlation_id", ...)` upstream of this
        // call site. The pre-existing `tracing::debug!(verify_queue_depth = ...)`
        // event below becomes a child event of this span automatically.
        let span = tracing::info_span!("lunaris.recall", correlation_id = tracing::field::Empty);
        async move {
            let threshold = std::env::var(ENV_VERIFY_WARN_THRESHOLD)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(DEFAULT_VERIFY_WARN_THRESHOLD);

            // RFC 0001: queue_depth here is a handle-level health check (not
            // per-scope). `Scope::dev()` is intentional — the verify queue is
            // a global topic shared across scopes. Per-scope queue depth routing
            // would require ScopedLunaris::recall_with_degraded_check (Wave 1E).
            // scope-dev-allowed: bare-recall-queue-depth — verify topic is global,
            // per-scope routing deferred to Wave 1E.
            let degraded_signal = match self
                .storage
                .queue_depth(&lunaris_core::Scope::dev(), VERIFY_TOPIC, 0)
                .await
            {
                Ok(depth) => {
                    tracing::debug!(
                        verify_queue_depth = depth,
                        threshold,
                        "recall_queue_depth_check"
                    );
                    depth > threshold
                }
                Err(e) => {
                    // Backend doesn't implement queue_depth (or transient
                    // failure). Don't fail the recall — fall through with
                    // degraded=false so the recall still serves results.
                    tracing::debug!(err = %e, "recall_queue_depth_unavailable; degraded=false");
                    false
                }
            };

            let mut b = self.recall();
            if degraded_signal {
                b = b.with_initial_degraded(true);
            }
            Ok(b)
        }
        .instrument(span)
        .await
    }
}
