//! lunaris-verify — Phase 4 slow-path arbitration verifier (Plan 04-01).
//!
//! Per blueprint §5.1 the v0 verifier is **default-OFF**: the umbrella
//! `Lunaris` handle constructs a [`NoopVerifier`] and the worker thread is
//! NOT spawned until `handle.verify_pipeline().enable()` is called (Plan 04-04).
//!
//! Four feature-gated backends behind one dyn-compatible [`Verifier`] trait:
//!
//! - [`NoopVerifier`] (unconditional, default when the pipeline is OFF or
//!   when no backend is wired via `with_verifier`) — returns a "deferred"
//!   [`VerifyDecision`] that the worker treats as "skip this item" without
//!   issuing the MVCC supersede write.
//! - `CandleGemma3_27B` (`feature = "candle"`) — Gemma-3 27B instruction-tuned
//!   verifier via candle 0.10; much slower (7-10x) than the 4B extractor so
//!   the per-chunk + batch timeouts are bumped accordingly.
//! - `OllamaVerifier` (`feature = "ollama"`) — POSTs `/api/chat` with a JSON-
//!   schema `format` field restricting the model to the `{winner_id, reason}`
//!   arbitration shape.
//! - `CloudApiVerifier` (`feature = "cloud-api"`) — provider-mux for Anthropic
//!   (claude-3-5-sonnet-latest default — sonnet not haiku because verifier is
//!   the slow-path "get it right" model), OpenAI (gpt-4o), and Gemini
//!   (gemini-1.5-pro). D-21 single-retry-then-flag preserved.
//!
//! ## Default-OFF contract (blueprint §5.1)
//!
//! `default = []` in `Cargo.toml` ([features] section). v0 ships verifier
//! OFF; loading a 27B model by default is a non-starter on dev hardware.
//! Plan 04-04 wires the `VerifierPipelineHandle::enable()` toggle that
//! spawns the worker via `run_verify_worker`.
//!
//! ## Worker subscribe + MVCC supersede (Plan 04-01 Task 3 + Plan 04-04 Task 4)
//!
//! [`worker::run_verify_worker`] subscribes to `__lunaris_verify__` partition
//! 0 with consumer group `lunaris-verify-v0` (D-06). Every successful
//! [`VerifyDecision`] flows through ONE `StoragePort::atomic_write` call
//! (D-11 MVCC supersede invariant) followed by one fire-and-forget audit
//! publish to `__lunaris_audit__` (D-22). Plan 04-04 Task 4 replaces this
//! plan's synthetic-key `apply_supersede` stub with the real primitive-row
//! supersede via `read_as_of` + `BiTemporal::invalidate_sys`.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

use async_trait::async_trait;
use lunaris_core::LunarisError;

#[cfg(feature = "candle")]
pub mod candle_gemma3_27b;
#[cfg(feature = "cloud-api")]
pub mod cloud_api;
pub mod noop;
#[cfg(feature = "ollama")]
pub mod ollama;
pub mod types;
pub mod worker;

#[cfg(feature = "candle")]
pub use candle_gemma3_27b::{CandleGemma3_27B, CandleGemma3_27BOpts};
#[cfg(feature = "cloud-api")]
pub use cloud_api::{CloudApiVerifier, CloudApiVerifierOpts, CloudProvider};
pub use noop::NoopVerifier;
#[cfg(feature = "ollama")]
pub use ollama::{OllamaVerifier, OllamaVerifierOpts};
pub use types::{VerifierBackend, VerifyDecision};
pub use worker::{VERIFY_CONSUMER_GROUP, VERIFY_TOPIC, run_verify_worker};

// Re-export the NeedsReview DTOs so downstream callers don't need a direct
// path dep on `lunaris-extract` just to consume the verifier trait surface.
pub use lunaris_extract::{NeedsReviewItem, NeedsReviewReason};

/// Object-safe async verifier.
///
/// Per blueprint §5.1 + D-01 the v0 default implementation is
/// [`NoopVerifier`]; alternative backends include `CandleGemma3_27B`,
/// `OllamaVerifier`, and `CloudApiVerifier`. All implementations MUST honour
/// the [`applies`](Verifier::applies) contract — `false` signals the worker
/// that the verifier is a passthrough and MUST short-circuit the MVCC
/// supersede write (Plan 04-01 Task 3 worker loop).
///
/// `Arc<dyn Verifier>` is constructible (proven by the compile-time
/// `verifier_is_dyn_compat` test), so the Plan 04-04
/// `VerifierPipelineHandle::set_verifier` builder accepts any backend without
/// compile-time monomorphization.
#[async_trait]
pub trait Verifier: Send + Sync + 'static {
    /// Arbitrate one [`NeedsReviewItem`] produced by the Phase 3 Validator
    /// OR detected here as a cross-Episode contradiction. Returns a
    /// [`VerifyDecision`] naming the winner + loser + reason (D-11 MVCC
    /// supersede).
    ///
    /// Returning `Ok(VerifyDecision::deferred())` signals "abstain"; the
    /// worker skips the atomic_write for deferred decisions. Returning
    /// `Err(...)` signals a hard failure; the worker logs + nacks the
    /// message so the broker can redeliver.
    async fn verify(
        &self,
        item: NeedsReviewItem,
    ) -> Result<VerifyDecision, LunarisError>;

    /// Returns `true` when this verifier produces real arbitrations; `false`
    /// for [`NoopVerifier`] so the worker can short-circuit before calling
    /// [`verify`](Verifier::verify) entirely.
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
    /// `Arc<dyn Verifier>` form on the umbrella handle stops compiling.
    #[test]
    fn verifier_is_dyn_compat() {
        fn _check<T: Verifier + ?Sized>() {}
        _check::<dyn Verifier>();
        let _: Arc<dyn Verifier> = Arc::new(NoopVerifier);
    }
}
