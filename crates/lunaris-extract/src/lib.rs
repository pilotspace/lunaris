//! lunaris-extract — Phase 3 entity + relation + fact extractor (default-OFF
//! per blueprint §5.2).
//!
//! Three feature-gated backends behind one dyn-compatible [`Extractor`] trait:
//!
//! - [`CandleGemma3_4B`] (default, `feature = "candle"`) — Gemma-3 4B
//!   instruction-tuned, loaded via candle 0.10's typed
//!   `candle_transformers::models::gemma3::Model`. Uses the safe
//!   `VarBuilder::from_buffered_safetensors` loader (CLAUDE.md no-unsafe rule).
//!   Per-batch timeout (~150 ms by default; D-02) falls back to per-chunk
//!   extraction on timeout — mirrors `lunaris-ingest::pipeline::embed_with_fallback`.
//! - `OllamaExtractor` (`feature = "ollama"`) — POSTs `/api/chat` with the
//!   GBNF grammar translated to a JSON schema in the `format` field.
//! - `CloudApiExtractor` (`feature = "cloud-api"`) — provider-mux Anthropic /
//!   OpenAI / Gemini selectable via `LUNARIS_EXTRACT_PROVIDER` env (D-01). Per
//!   D-21: single retry on transient errors then emit a sentinel entity that
//!   the [`validator::validate`] pass routes to
//!   `NeedsReviewReason::TransientAfterRetry`.
//!
//! ## Default-OFF contract (blueprint §5.2 + D-11)
//!
//! The umbrella `Lunaris` handle keeps the extractor under a
//! `GraphPipelineHandle` toggle (Plan 03-03). With the toggle OFF the extractor
//! is dead code: no model loaded, no candle device init, no extra threads
//! spawned. The crate compiles + tests succeed without any Gemma weights
//! present (the `missing_weights_returns_actionable_error` test is the
//! load-bearing assertion). Cache miss returns the actionable error
//! ```text
//! gemma-3-4b-it weights missing at PATH — run `huggingface-cli download google/gemma-3-4b-it --local-dir PATH`
//! ```
//! Plan 03-03 catches it and substitutes [`NoopExtractor`] with
//! `tracing::warn!`.
//!
//! ## EntityId derivation (D-06)
//!
//! [`EntityId`] is the deterministic 16-byte truncation of
//! `blake3(canonical_name_normalized || "::" || entity_type)`. Stable across
//! re-ingest, across chunks within an Episode, across Episodes. No second-pass
//! dedupe round trip. See [`types::EntityId::from_name_and_type`].
//!
//! ## Validator (D-08)
//!
//! [`validator::validate`] walks a [`RawExtractionBatch`] and routes invalid
//! items to [`validator::ValidatedExtraction::needs_review`] with one of four
//! structured reasons:
//!
//! 1. [`NeedsReviewReason::InvalidBitemporal`] — `valid_from >= valid_to`
//! 2. [`NeedsReviewReason::StructuralContradiction`] — same `(subject_id,
//!    predicate)` with overlapping `[valid_from, valid_to]` and conflicting
//!    `object_id`s within the SAME Episode
//! 3. [`NeedsReviewReason::GbnfFailure`] — parsed item violates the grammar
//!    schema (empty name / out-of-range confidence / etc.)
//! 4. [`NeedsReviewReason::TransientAfterRetry`] — cloud-api retry exhaust
//!    (D-21 sentinel detection)
//!
//! Cross-Episode contradictions (e.g., two Episodes claim different birth years
//! for Alice) are deferred to the Phase 4 Verifier worker per D-09 — out of
//! scope here.
//!
//! ## Module layout
//!
//! - [`types`] — DTOs ([`EntityId`], [`Entity`], [`Relation`], [`Fact`],
//!   [`ChunkInput`], [`RawExtraction`], [`RawExtractionBatch`],
//!   [`ExtractionBatch`])
//! - [`noop`] — [`NoopExtractor`] (always-empty extraction; default when
//!   graph pipeline is OFF or weights missing)
//! - [`candle_gemma3`] (gated `candle`) — [`CandleGemma3_4B`]
//! - `ollama` (gated `ollama`) — `OllamaExtractor`
//! - `cloud_api` (gated `cloud-api`) — `CloudApiExtractor`
//! - [`validator`] — [`validator::validate`] + `NeedsReviewReason`

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

use async_trait::async_trait;
use lunaris_core::LunarisError;
use ulid::Ulid;

#[cfg(feature = "candle")]
pub mod candle_gemma3;
#[cfg(feature = "cloud-api")]
pub mod cloud_api;
// RFC 0007 §3 — FallbackExtractor<P, F> static-dispatch combinator with
// per-instance CircuitBreaker. Always built; the breaker primitive lives
// in lunaris-core::circuit_breaker.
pub mod fallback;
pub mod noop;
#[cfg(feature = "ollama")]
pub mod ollama;
pub mod types;
pub mod validator;

#[cfg(feature = "candle")]
pub use candle_gemma3::{CandleGemma3_4B, CandleGemma3_4BOpts};
#[cfg(feature = "cloud-api")]
pub use cloud_api::{CloudApiExtractor, CloudApiExtractorOpts, CloudProvider};
pub use noop::NoopExtractor;
#[cfg(feature = "ollama")]
pub use ollama::{OllamaExtractor, OllamaExtractorOpts};
pub use types::{
    ChunkInput, Entity, EntityId, ExtractionBatch, Fact, RawExtraction, RawExtractionBatch,
    Relation,
};
pub use validator::{
    NeedsReviewItem, NeedsReviewReason, ValidatedExtraction, into_batch, validate,
};

/// Object-safe async extractor.
///
/// Per blueprint §5.2 + D-01 the default implementation is [`CandleGemma3_4B`];
/// alternative backends include `OllamaExtractor` (HTTP) and
/// `CloudApiExtractor` (Anthropic / OpenAI / Gemini). All implementations
/// MUST honour the per-batch timeout (D-02) by falling back to per-chunk
/// extraction on timeout, and MUST emit either valid extractions or a sentinel
/// recognized by [`validator::validate`] — never silently drop chunks.
///
/// `Arc<dyn Extractor>` is constructible (proven by the compile-time
/// `extractor_is_dyn_compat` test), so the umbrella `Lunaris::with_extractor`
/// builder accepts any backend without compile-time monomorphization.
#[async_trait]
pub trait Extractor: Send + Sync + 'static {
    /// Extract entities + relations + facts from a batch of chunk inputs.
    ///
    /// Returns a [`RawExtractionBatch`] — call [`validate`] downstream to flag
    /// NeedsReview cases (per EXTRACT-05). The Plan 03-03 ingest fan-out
    /// converts the validated batch into `WriteOp::GraphNode` /
    /// `WriteOp::GraphEdge` ops at the `StoragePort::atomic_write` boundary.
    async fn extract(
        &self,
        episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError>;

    /// Returns `true` when this extractor produces real extractions; `false`
    /// for [`NoopExtractor`] so callers (Plan 03-03 ingest fan-out) can skip
    /// the GraphNode / GraphEdge `WriteOp`s when `applies() == false`.
    fn applies(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Compile-time proof the trait is dyn-compatible (object-safe). If a
    /// future addition (generic method, `Self: Sized` bound) breaks this, the
    /// `Arc<dyn Extractor>` form on the umbrella handle stops compiling.
    #[test]
    fn extractor_is_dyn_compat() {
        fn _check<T: Extractor + ?Sized>() {}
        _check::<dyn Extractor>();
        let _: Arc<dyn Extractor> = Arc::new(NoopExtractor);
    }
}
