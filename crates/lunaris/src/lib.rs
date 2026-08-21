//! lunaris — umbrella crate. Re-exports `lunaris_core` types, exposes the
//! `open(url)` URL-scheme dispatcher AND the higher-level `Lunaris` handle
//! that drives the Phase 2 ingest hot path.
//!
//! ## Two construction paths
//!
//! - [`open()`](crate::open::open) — returns `Arc<dyn StoragePort>` for
//!   callers that just want raw storage access (Plan 5 conformance harness,
//!   low-level tests).
//! - [`Lunaris::open`](crate::handle::Lunaris::open) — returns a high-level
//!   handle wired with a default [`Embedder`] + [`HlcClock`] so callers can
//!   call `lunaris.ingest(episode).await?` without manually plumbing the
//!   Phase 2 pipeline. This is what Helios uses.
//!
//! ## Prelude
//!
//! For day-to-day use, glob-import the curated common surface:
//!
//! ```rust
//! use lunaris::prelude::*;
//! ```
//!
//! See [`prelude`] for the exact list — it intentionally stays small
//! (handle, scope, episode builder, retrieval DSL, the pluggable
//! trait + `Noop*` pairs, and the umbrella error type).
#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod audit;
pub mod consolidator_pipeline;
// RFC 0001 Wave 1D — EpisodeBuilder lives here (NOT in lunaris-core) so
// `into_episode` can be `pub(crate)`, enforcing that only ScopedLunaris::ingest
// can stamp a scope onto an episode.
pub mod digest;
pub mod episode_builder;
pub mod forget;
pub mod graph_pipeline;
pub mod handle;
pub mod ingest;
// W2-L2 — bulk FT invalidation for Helios force-push recovery (UC-G3).
// Internal fan-out logic; public surface is `Lunaris::invalidate_range`.
pub(crate) mod invalidate;
// Plan 05-05 OPS-08 — `lunaris::logging::init()` JSON-vs-pretty subscriber
// selector helper. Production triggers per CONTEXT.md D-26: `LUNARIS_ENV=production`
// OR `!std::io::stdout().is_terminal()`. Re-exported as `init_logging` below
// so embedded callers can `use lunaris::init_logging;` without reaching into
// the module path.
pub mod logging;
pub mod open;
// Phase 12 Option-A relocation: `WorkingMemory` moved here from
// `lunaris-recipes` so `HeliosScratchpad` (also in this crate) can compose
// over it without a dependency cycle. `lunaris-recipes` re-exports the type
// so Phase 9/10/11 callers using `lunaris_recipes::WorkingMemory` compile
// unchanged. Phase 13 proper primitives-crate extraction subsumes this.
pub mod primitives;
pub mod recall;
// GA-1 — opt-in cross-encoder rerank stage on the production recall root
// (`LUNARIS_RECALL_RERANK`, read once at handle construction).
pub mod recall_rerank;
pub mod recipes;
// `memory-update-intelligence` — pure cross-episode reconciliation decision
// core (dedup NOOP / additive Append / cross-episode Supersede). Consumed by
// `structured_ingest` to converge memories without copying Mem0's
// LLM-mutate-on-write; bi-temporal MVCC stays the source of truth.
pub mod reconcile;
// Plan 08-00 — `Lunaris::snapshot()` monotonic LSN marker. Pre-req for Plan
// 08-01 codegen so the `snapshot` surface entry has a real inherent method
// to bind from PyO3 + napi-rs. The module only contains an `impl Lunaris`
// block + `#[cfg(test)] mod tests`, so no extra `pub use` is required.
pub mod snapshot;
// Phase 23 — agent-facing structured ingest. Reuses the same INGEST-04
// single-atomic_write invariant as `ingest` but skips the LLM extractor.
pub mod structured_ingest;
pub mod verify_pipeline;

pub use audit::{AUDIT_TOPIC, AuditEvent, publish_audit_event};
pub use consolidator_pipeline::{
    ConsolidatorPipelineHandle, ENABLED_ENV_VAR as CONSOLIDATE_ENABLED_ENV_VAR,
};
pub use digest::recent_by_source;
pub use episode_builder::EpisodeBuilder;
pub use forget::{ForgetConfirmation, ForgetReceipt, ForgetTarget, IndexKind, ScopeSpec};
pub use graph_pipeline::{ENABLED_ENV_VAR as GRAPH_ENABLED_ENV_VAR, GraphPipelineHandle};
pub use handle::{
    EmbedderBackend, IngestKind, Lunaris, ScopedLunaris, VerifyAgendaEntry, lazy_default_embedder,
    resolve_default_embedder, resolve_default_reranker, resolved_embedder_backend,
};
pub use recall_rerank::{RECALL_RERANK_ENV_VAR, RECALL_RERANK_TOP_IN_ENV_VAR, RecallRerankConfig};
// Phase 23 — agent-facing structured-ingest public surface.
pub use structured_ingest::{EntityInput, FactInput, RelationInput, StructuredIngest};
// Phase 12 Option-A: `WorkingMemory` lives here now. `lunaris-recipes`
// re-exports this path so the established `lunaris_recipes::WorkingMemory`
// import stays stable.
pub use primitives::WorkingMemory;
// Plan 05-05 OPS-08 — re-export `lunaris::logging::init` as
// `lunaris::init_logging` for the canonical embedded-caller use site
// `lunaris::init_logging();`. The `lunaris-server` binary calls
// `lunaris::logging::init()` via the full path; both are equivalent.
pub use logging::init as init_logging;
pub use lunaris_core::*;
pub use open::open;
// `bootstrap_app_role` / `BootstrapReport` (the Postgres production-role
// bootstrap behind `lunaris-server bootstrap-db`) were removed in 0.7.0 with
// `lunaris-storage-postgres`. Moon has no role/RLS bootstrap step — see
// docs/operations/external-moon.md.
// Plan 05-04 — opinionated v0 recipes (helios-rfc §5.3 surface). v0 ships only
// CodingSessionMemory (renamed from HeliosScratchpad in v0.5) + its borrowed
// AsOfScratchpad time-travel view; the other 9 recipes (RECIPE-V1-01..11) ship in v1.
/// Deprecated alias for [`CodingSessionMemory`]. Remove in v0.7.
// allow(deprecated): re-exporting the deprecated alias is the whole point —
// without the allow, the defining crate trips its own deprecation lint and
// `clippy -D warnings` fails. Downstream importers still get the warning.
#[allow(deprecated)]
#[deprecated(
    since = "0.5.0",
    note = "use CodingSessionMemory; HeliosScratchpad will be removed in v0.7"
)]
pub use recipes::HeliosScratchpad;
pub use recipes::{AsOfScratchpad, CodingSessionMemory};
pub use verify_pipeline::{ENABLED_ENV_VAR as VERIFY_ENABLED_ENV_VAR, VerifierPipelineHandle};

// Plan 04 — verifier + consolidator trait surface re-exports so callers
// `use lunaris::{Verifier, Consolidator, NoopVerifier, NoopConsolidator}`
// without reaching into the per-crate paths.
// T1d (260609-dvi): ActRConsolidator re-exported so lunaris-mcp needs no
// direct dep on lunaris-consolidate.
pub use lunaris_consolidate::{
    ActRConsolidator, Consolidator, DreamAgenda, DreamCluster, DreamConfig, NoopConsolidator,
};
pub use lunaris_verify::{
    NeedsReviewItem as VerifyNeedsReviewItem, NoopVerifier, Verifier, VerifierBackend,
    VerifyDecision,
};
// Phase 13 — per-turn reflect supervisor re-exports. Callers
// `use lunaris::{ReflectSupervisor, NoopReflectSupervisor, LlmReflectSupervisor,
//                ReflectInput, ReflectOutput, ReflectOpts}`
// without reaching into lunaris-verify directly.
pub use lunaris_verify::{
    LlmReflectSupervisor, NoopReflectSupervisor, ReflectInput, ReflectOpts, ReflectOutput,
    ReflectSupervisor,
};

// Cfg-gated verifier backends — mirror the extract backends gating pattern.
#[cfg(feature = "cloud-api")]
pub use lunaris_verify::{CloudApiVerifier, CloudApiVerifierOpts};
#[cfg(feature = "ollama")]
pub use lunaris_verify::{OllamaVerifier, OllamaVerifierOpts};

// Phase 2 retrieve DSL re-exports — callers `use lunaris::{Vector, Keyword, ...}`
// rather than reaching into `lunaris_retrieve::`.
//
// Plan 03-02 added `Graph` to the retrieve crate; the umbrella crate forwards
// it here so callers `use lunaris::{Graph, EntityId}` for the canonical
// blueprint §8 compose example.
//
// N5/B2 added the RAPTOR `Tree` operator; forwarded here so callers
// `use lunaris::{Tree, Vector}` and never reach into `lunaris_retrieve::`
// (the retrieval-DSL guide's standing contract).
pub use lunaris_retrieve::{
    AndRetriever, DEFAULT_GRAPH_HOPS, DEFAULT_GRAPH_K, DegradedFallbackRetriever,
    FlooredTopRetriever, FuseRrfRetriever, Graph, Hit, Keyword, LUNARIS_GRAPH_NAME, MAX_GRAPH_HOPS,
    Navigate, Plan, Query, RawHit, RerankRetriever, RetrievalBuilder, RetrievalService, Retriever,
    SourceOp, TopRetriever, Tree, Vector, degraded_fallback, filter_str, floored_top, hybrid_root,
    plan_query, rerank,
};

// Reranker trait + helpers re-exported from lunaris-rerank so callers
// `use lunaris::{Reranker, NoopReranker}`. The concrete cross-encoder
// (`LlamaCppReranker`) lives in `lunaris-llamacpp` — operators who want
// to construct it directly import from that crate; callers who only need
// the trait + Noop seam stay on this re-export. v0.4 N-03 cutover.
pub use lunaris_rerank::{NoopReranker, RerankCandidate, Reranker};

// Plan 03-03: Extractor trait + helpers re-exported from lunaris-extract so
// callers `use lunaris::{Extractor, NoopExtractor, EntityId}` for the
// canonical compose example. Following the W-8 fix, we re-export ONLY the
// trait + ID newtype + Noop impl + Validator outputs at the umbrella level.
// Callers wanting extract DTOs directly use `lunaris_extract::{Entity,
// Relation, Fact}` — namespacing prevents collision with `lunaris_core`'s
// storage primitives that share those names.
pub use lunaris_extract::{
    EntityId, Extractor, NeedsReviewItem, NeedsReviewReason, NoopExtractor, ValidatedExtraction,
};

// Cfg-gated extractor backends. A `cargo check --no-default-features`
// build pulls no http stack.
#[cfg(feature = "cloud-api")]
pub use lunaris_extract::{CloudApiExtractor, CloudApiExtractorOpts, CloudProvider};
#[cfg(feature = "ollama")]
pub use lunaris_extract::{OllamaExtractor, OllamaExtractorOpts};

// Re-export the backend concrete type for callers who want to construct
// directly (bypassing URL routing — needed by the conformance harness in
// Phase 5). Since 0.7.0 there is exactly one.
pub use lunaris_storage_moon::MoonStorage;

/// Glob-import the common surface: `use lunaris::prelude::*;`.
///
/// This is a *curated* set — the handful of symbols a typical caller needs to
/// open a store, build an episode, run a retrieval, and plug in (or stub out)
/// the optional pipeline stages. It is deliberately **not** a glob of every
/// re-export; reach into `lunaris::` directly for the long tail (pipeline
/// handles, backend opts, conformance helpers, env-var constants, …).
pub mod prelude {
    // Handle + scoping.
    pub use crate::handle::{Lunaris, ScopedLunaris};
    pub use lunaris_core::scope::Scope;

    // Building an episode + targeting a forget.
    pub use crate::episode_builder::EpisodeBuilder;
    pub use crate::forget::{ForgetTarget, ScopeSpec};

    // Retrieval DSL.
    pub use lunaris_retrieve::{Graph, Hit, Keyword, Query, RetrievalBuilder, Tree, Vector};

    // Umbrella error type.
    pub use lunaris_core::error::LunarisError;

    // Pluggable trait surface + their Noop fallbacks.
    pub use lunaris_consolidate::{Consolidator, NoopConsolidator};
    pub use lunaris_core::embedder::Embedder;
    pub use lunaris_core::hlc::HlcClock;
    pub use lunaris_extract::{Extractor, NoopExtractor};
    pub use lunaris_rerank::{NoopReranker, Reranker};
    pub use lunaris_verify::{NoopVerifier, Verifier};
}
