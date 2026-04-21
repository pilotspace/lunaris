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

impl Lunaris {
    /// Build a [`RetrievalBuilder`] bound to this handle's storage, keyword,
    /// embedder, and (when available) the typed `Arc<MoonStorage>` that
    /// enables the Phase 1.5 RRF Moon-native dispatch.
    ///
    /// Default root operator is `Vector::new("chunks", 30)`. Callers replace
    /// the root via [`RetrievalBuilder::with_root`] for the canonical example
    /// from blueprint §8:
    ///
    /// ```ignore
    /// let hits = lunaris.recall()
    ///     .with_root(Vector::new("chunks", 30)
    ///         .and(Keyword::bm25("chunks", 30))
    ///         .fuse_rrf(60)
    ///         .top(5))
    ///     .filter_str("source LIKE 'helios:fs/%'").unwrap()
    ///     .execute(Query::text("brown fox"))
    ///     .await?;
    /// ```
    ///
    /// Plan 03-03 graph-aware extension — once `handle.graph_pipeline().enable()`
    /// is called and Episodes have been ingested with extracted Entities,
    /// callers can compose `Graph::anchored` into the same DSL:
    ///
    /// ```ignore
    /// use lunaris::{EntityId, Graph, Query, Vector};
    /// let alice = EntityId::from_name_and_type("Alice", "Person");
    /// let hits = lunaris.recall()
    ///     .with_root(Vector::new("chunks", 30)
    ///         .and(Graph::anchored(vec![alice], 2))
    ///         .fuse_rrf(60)
    ///         .rerank(lunaris.reranker())
    ///         .top(5))
    ///     .execute(Query::text("Tell me about Alice"))
    ///     .await?;
    /// ```
    pub fn recall(&self) -> RetrievalBuilder {
        let mut b = RetrievalBuilder::from_handle(self.storage(), self.keyword(), self.embedder());
        if let Some(moon) = self.moon_storage() {
            b = b.with_moon_storage(moon);
        }
        b
    }

    /// Plan 04-04 B-8 fix: NEW async variant that reads the verifier queue
    /// depth ONCE per call and seeds the resulting [`RetrievalBuilder`] with
    /// `with_initial_degraded(true)` when the depth crosses
    /// [`LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD`] (default 1000).
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

            let degraded_signal = match self.storage.queue_depth(VERIFY_TOPIC, 0).await {
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
