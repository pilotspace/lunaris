//! lunaris-rerank — cross-encoder reranker for the v0 recall hot path (RETRIEVE-06).
//!
//! Per blueprint §4.2 latency budget the recall p50 ≤ 25 ms target ALLOCATES
//! 12 ms to a cross-encoder rerank pass. Without it, the recall budget is
//! fictional. This crate ships:
//!
//! - [`Reranker`] trait — async, dyn-compatible. Takes a query + docs, returns
//!   the same set of docs re-scored and re-ordered.
//! - [`BgeRerankerV2M3`] (default, behind `candle` feature) — bge-reranker-v2-m3
//!   cross-encoder loaded via candle 0.10 on CPU.
//! - [`NoopReranker`] — passthrough fallback used when the model weights are
//!   unavailable. Per RETRIEVE-06 contract: the operator code path MUST be
//!   present even when the model is missing — that's what makes the dev box
//!   without 1.1 GB of cached weights still work.
//!
//! ## Cold-start contract
//!
//! `BgeRerankerV2M3::try_new_from_default_cache()` returns
//! `Result<Self, LunarisError>`. The umbrella `Lunaris::open(url)` catches the
//! `Err` from a cache miss and substitutes [`NoopReranker`], emitting
//! `tracing::warn!` so the operator sees the degradation. Callers can wire
//! their own reranker via `Lunaris::with_reranker(reranker)`.
//!
//! ## Latency budget
//!
//! The rerank pass receives the top-30 candidates from the upstream retriever
//! and returns them re-ranked; the operator's downstream `.top(k)` modifier
//! truncates to the user-facing k (typically 5). Per blueprint §4.2 the budget
//! is 12 ms p50 / 35 ms p99 on CPU.
//!
//! ## Module layout
//!
//! - [`bge`] — `BgeRerankerV2M3` (gated `candle`)
//! - [`noop`] — `NoopReranker`

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

use async_trait::async_trait;
use lunaris_core::LunarisError;
use serde::{Deserialize, Serialize};

#[cfg(feature = "candle")]
pub mod bge;
pub mod noop;

#[cfg(feature = "candle")]
pub use bge::{BgeRerankerV2M3, BgeRerankerV2M3Opts};
pub use noop::NoopReranker;

/// One pre-rerank candidate. The operator hydrates the chunk text BEFORE
/// calling the reranker (see `lunaris_retrieve::hydrate::partial_hydrate_text`)
/// so the cross-encoder can pair-encode `(query, doc.text)` for scoring.
///
/// `score` is the upstream operator's score — the reranker REPLACES this with
/// its own logit so downstream `.top(n)` ranks by cross-encoder relevance.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RerankCandidate {
    /// Backend-issued id (the same id bytes that came from the upstream RawHit).
    pub id: Vec<u8>,
    /// Chunk body — required for cross-encoder pair scoring. Empty string is
    /// allowed (the model will produce a low score, the cull is downstream).
    pub text: String,
    /// Upstream operator's score. The reranker REPLACES this with its own
    /// logit on return.
    pub score: f32,
    /// Free-form metadata carried from the upstream RawHit; the reranker
    /// passes it through unchanged.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Async cross-encoder reranker.
///
/// Implementors MUST return exactly `docs.len()` items — preserving the input
/// set, just re-ordered + re-scored. The retriever's `top(n)` modifier
/// truncates downstream so the reranker doesn't need to know the user-facing k.
///
/// `applies()` reports whether this impl actually applies a model pass (true)
/// or is a NO-OP passthrough (false). The DSL operator reads this to set
/// `Hit { rerank_applied }` so callers can tell whether they got the budgeted
/// 12 ms cross-encoder pass or the degraded path.
#[async_trait]
pub trait Reranker: Send + Sync + 'static {
    /// Re-score (query, docs) pairs and return them sorted by score desc.
    async fn rerank(
        &self,
        query: &str,
        docs: Vec<RerankCandidate>,
    ) -> Result<Vec<RerankCandidate>, LunarisError>;

    /// True when this impl actually invokes a model; false for NO-OP fallbacks.
    /// The DSL operator reads this to set `Hit { rerank_applied }`.
    fn applies(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Compile-time proof the trait is dyn-compatible (object-safe). If this
    /// stops compiling we've broken the operator wiring contract.
    #[test]
    fn reranker_is_dyn_compat() {
        fn _check<T: Reranker + ?Sized>() {}
        _check::<dyn Reranker>();
        let _: Arc<dyn Reranker> = Arc::new(NoopReranker);
    }
}
