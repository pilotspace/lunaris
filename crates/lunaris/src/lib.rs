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
#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod audit;
pub mod consolidator_pipeline;
// RFC 0001 Wave 1D — EpisodeBuilder lives here (NOT in lunaris-core) so
// `into_episode` can be `pub(crate)`, enforcing that only ScopedLunaris::ingest
// can stamp a scope onto an episode.
pub mod episode_builder;
pub mod forget;
pub mod graph_pipeline;
pub mod handle;
pub mod ingest;
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
pub mod recipes;
// Plan 08-00 — `Lunaris::snapshot()` monotonic LSN marker. Pre-req for Plan
// 08-01 codegen so the `snapshot` surface entry has a real inherent method
// to bind from PyO3 + napi-rs. The module only contains an `impl Lunaris`
// block + `#[cfg(test)] mod tests`, so no extra `pub use` is required.
pub mod snapshot;
pub mod verify_pipeline;

pub use audit::{AUDIT_TOPIC, AuditEvent, publish_audit_event};
pub use consolidator_pipeline::{
    ConsolidatorPipelineHandle, ENABLED_ENV_VAR as CONSOLIDATE_ENABLED_ENV_VAR,
};
pub use forget::{ForgetConfirmation, ForgetReceipt, ForgetTarget, IndexKind, ScopeSpec};
pub use graph_pipeline::{ENABLED_ENV_VAR as GRAPH_ENABLED_ENV_VAR, GraphPipelineHandle};
pub use episode_builder::EpisodeBuilder;
pub use handle::{Lunaris, ScopedLunaris};
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
// Plan 05-04 — opinionated v0 recipes (helios-rfc §5.3 surface). v0 ships only
// HeliosScratchpad + its borrowed AsOfScratchpad time-travel view; the other
// 9 recipes (RECIPE-V1-01..11) ship in v1.
pub use recipes::{AsOfScratchpad, HeliosScratchpad};
pub use verify_pipeline::{ENABLED_ENV_VAR as VERIFY_ENABLED_ENV_VAR, VerifierPipelineHandle};

// Plan 04 — verifier + consolidator trait surface re-exports so callers
// `use lunaris::{Verifier, Consolidator, NoopVerifier, NoopConsolidator}`
// without reaching into the per-crate paths.
pub use lunaris_consolidate::{Consolidator, NoopConsolidator};
pub use lunaris_verify::{
    NeedsReviewItem as VerifyNeedsReviewItem, NoopVerifier, Verifier, VerifierBackend,
    VerifyDecision,
};

// Cfg-gated verifier backends — mirror the extract backends gating pattern.
#[cfg(feature = "candle")]
pub use lunaris_verify::{CandleGemma3_27B, CandleGemma3_27BOpts};
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
pub use lunaris_retrieve::{
    DEFAULT_GRAPH_HOPS, DEFAULT_GRAPH_K, DegradedFallbackRetriever, Graph, Hit, Keyword,
    LUNARIS_GRAPH_NAME, MAX_GRAPH_HOPS, Plan, Query, RawHit, RerankRetriever, RetrievalBuilder,
    RetrievalService, SourceOp, Vector, degraded_fallback, filter_str, plan_query, rerank,
};

// Plan 02-03: Reranker trait + helpers re-exported from lunaris-rerank so
// callers `use lunaris::{Reranker, NoopReranker}`. `BgeRerankerV2M3` is gated
// behind the `candle` feature so a `cargo check --no-default-features` build
// doesn't pull the candle stack.
#[cfg(feature = "candle")]
pub use lunaris_rerank::{BgeRerankerV2M3, BgeRerankerV2M3Opts};
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

// Cfg-gated extractor backends — mirror the `BgeRerankerV2M3` gating pattern
// at lines 38-39. A `cargo check --no-default-features` build pulls neither
// the candle stack nor the http stack.
#[cfg(feature = "candle")]
pub use lunaris_extract::{CandleGemma3_4B, CandleGemma3_4BOpts};
#[cfg(feature = "cloud-api")]
pub use lunaris_extract::{CloudApiExtractor, CloudApiExtractorOpts, CloudProvider};
#[cfg(feature = "ollama")]
pub use lunaris_extract::{OllamaExtractor, OllamaExtractorOpts};

// Re-export backend concrete types for callers who want to construct directly
// (bypassing URL routing — needed by the conformance harness in Phase 5).
pub use lunaris_storage_moon::MoonStorage;
pub use lunaris_storage_postgres::PostgresStorage;
