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

pub mod graph_pipeline;
pub mod handle;
pub mod ingest;
pub mod open;
pub mod recall;

pub use graph_pipeline::{ENABLED_ENV_VAR as GRAPH_ENABLED_ENV_VAR, GraphPipelineHandle};
pub use handle::Lunaris;
pub use lunaris_core::*;
pub use open::open;

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
#[cfg(feature = "ollama")]
pub use lunaris_extract::{OllamaExtractor, OllamaExtractorOpts};
#[cfg(feature = "cloud-api")]
pub use lunaris_extract::{CloudApiExtractor, CloudApiExtractorOpts, CloudProvider};

// Re-export backend concrete types for callers who want to construct directly
// (bypassing URL routing — needed by the conformance harness in Phase 5).
pub use lunaris_storage_moon::MoonStorage;
pub use lunaris_storage_postgres::PostgresStorage;
