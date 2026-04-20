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

pub mod handle;
pub mod ingest;
pub mod open;
pub mod recall;

pub use handle::Lunaris;
pub use lunaris_core::*;
pub use open::open;

// Phase 2 retrieve DSL re-exports — callers `use lunaris::{Vector, Keyword, ...}`
// rather than reaching into `lunaris_retrieve::`.
pub use lunaris_retrieve::{
    DegradedFallbackRetriever, Hit, Keyword, Plan, Query, RawHit, RerankRetriever,
    RetrievalBuilder, RetrievalService, SourceOp, Vector, degraded_fallback, filter_str,
    plan_query, rerank,
};

// Plan 02-03: Reranker trait + helpers re-exported from lunaris-rerank so
// callers `use lunaris::{Reranker, NoopReranker}`. `BgeRerankerV2M3` is gated
// behind the `candle` feature so a `cargo check --no-default-features` build
// doesn't pull the candle stack.
#[cfg(feature = "candle")]
pub use lunaris_rerank::{BgeRerankerV2M3, BgeRerankerV2M3Opts};
pub use lunaris_rerank::{NoopReranker, RerankCandidate, Reranker};

// Re-export backend concrete types for callers who want to construct directly
// (bypassing URL routing — needed by the conformance harness in Phase 5).
pub use lunaris_storage_moon::MoonStorage;
pub use lunaris_storage_postgres::PostgresStorage;
