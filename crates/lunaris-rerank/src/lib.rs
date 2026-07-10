//! lunaris-rerank — trait + NoopReranker for the v0 recall hot path (RETRIEVE-06).
//!
//! v0.4 N-03 cutover: this crate slimmed from "default backend host" to
//! "trait + Noop seam." The concrete cross-encoders moved out:
//!
//! - `BgeRerankerV2M3` (candle) is deleted. The replacement is
//!   `lunaris_llamacpp::LlamaCppReranker` (llama.cpp + bge-reranker-v2-m3
//!   FP32, sigmoid output ∈ [0, 1]).
//! - `FastembedReranker` (ORT) and the `fastembed_exec` EP helper are deleted
//!   with the rest of the fastembed transitive surface.
//!
//! What stays:
//!
//! - [`Reranker`] trait — async, dyn-compatible. Implemented by
//!   `NativeReranker` (default), `NativeQuantizedReranker` (Q4 GGUF), and
//!   downstream BYO impls.
//! - [`RerankCandidate`] DTO — input/output of `Reranker::rerank`.
//! - [`NoopReranker`] — passthrough fallback for the RETRIEVE-06 contract.
//!
//! ## Cold-start contract
//!
//! The umbrella `Lunaris::open(url)` catches `Err` from
//! `NativeReranker::open` (cache miss) and substitutes [`NoopReranker`],
//! emitting `tracing::warn!` so the operator sees the degradation. Callers
//! wire their own reranker via `Lunaris::with_reranker(reranker)`.
//!
//! ## Latency budget
//!
//! Per blueprint §4.2 the budget is 12 ms p50 / 35 ms p99 on CPU.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

use async_trait::async_trait;
use lunaris_core::LunarisError;
use serde::{Deserialize, Serialize};

pub mod noop;

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
