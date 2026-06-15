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
//! `default = []` in `Cargo.toml` (`[features]` section). v0 ships verifier
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

#[cfg(feature = "verify-small")]
pub mod candle_gemma3_270m;
#[cfg(feature = "candle")]
pub mod candle_gemma3_27b;
#[cfg(feature = "cloud-api")]
pub mod cloud_api;
pub mod divergence;
// Phase 11 — backend-agnostic Verifier over `lunaris_llm::LlmBackend`. Additive;
// legacy CandleGemma3_27B / CandleGemma3_270M / OllamaVerifier /
// CloudApiVerifier remain untouched. Default verify model stays 27B; the
// flip to 4B is gated on ER-F1 and lands in a separate later commit.
pub mod llm_verifier;
pub mod noop;
// Phase 11 — per-turn reflection supervisor. Scaffold only in this commit:
// DTOs + trait + LlmReflectSupervisor impl. Turn-end wire-up to the umbrella
// `Lunaris` handle is a follow-up once a turn boundary signal is exposed.
#[cfg(feature = "ollama")]
pub mod ollama;
pub mod reflect;
// Phase 14.1 — MVCC invalidation helper for reflect-nominated facts.
pub mod reflect_apply;
pub mod supervisor;
pub mod types;
pub mod worker;

#[cfg(feature = "candle")]
pub use candle_gemma3_27b::{CandleGemma3_27B, CandleGemma3_27BOpts};
#[cfg(feature = "verify-small")]
pub use candle_gemma3_270m::{CandleGemma3_270M, CandleGemma3_270MOpts};
#[cfg(feature = "cloud-api")]
pub use cloud_api::{CloudApiVerifier, CloudApiVerifierOpts, CloudProvider};
pub use llm_verifier::{LlmVerifier, LlmVerifierOpts};
pub use noop::NoopVerifier;
#[cfg(feature = "ollama")]
pub use ollama::{OllamaVerifier, OllamaVerifierOpts};
pub use reflect::{
    LlmReflectSupervisor, NoopReflectSupervisor, ReflectInput, ReflectOpts, ReflectOutput,
    ReflectSupervisor,
};
pub use reflect_apply::{
    BOOST_DELTA, apply_reflect_boost, apply_reflect_invalidate, boost_cache_capacity,
};
pub use supervisor::{
    ENV_SCOPE_CONCURRENCY, ENV_SCOPE_IDLE_TIMEOUT_MS, VerifySupervisor, VerifySupervisorHandle,
    scope_verify_topic,
};
pub use types::{VerifierBackend, VerifyDecision, cross_episode_decision};
#[allow(deprecated)]
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
    async fn verify(&self, item: NeedsReviewItem) -> Result<VerifyDecision, LunarisError>;

    /// Returns `true` when this verifier produces real arbitrations; `false`
    /// for [`NoopVerifier`] so the worker can short-circuit before calling
    /// [`verify`](Verifier::verify) entirely.
    fn applies(&self) -> bool {
        true
    }
}

/// Shared arbitration prompt used by the candle + ollama + cloud-api
/// backends. v0 is intentionally minimal — a prompt-engineering iteration
/// pass is a v1 concern (CONSOL-V1-01 / VERIFY-V1-01).
///
/// The model is expected to emit a JSON object of shape
/// `{"winner_id": "<ulid>", "loser_id": "<ulid>", "reason": "..."}`. Any
/// other shape (including missing fields) is treated as a "deferred"
/// decision — the worker then skips the MVCC supersede for that item.
#[doc(hidden)]
#[allow(dead_code)]
pub(crate) fn arbitration_prompt(item: &NeedsReviewItem) -> String {
    let kind = match item {
        NeedsReviewItem::Entity { .. } => "entity",
        NeedsReviewItem::Relation { .. } => "relation",
        NeedsReviewItem::Fact { .. } => "fact",
    };
    let reason = match item {
        NeedsReviewItem::Entity { reason, .. } => format!("{reason}"),
        NeedsReviewItem::Relation { reason, .. } => format!("{reason}"),
        NeedsReviewItem::Fact { reason, .. } => format!("{reason}"),
    };
    let item_json = match item {
        NeedsReviewItem::Entity { raw, .. } => {
            serde_json::to_string(raw).unwrap_or_else(|_| "{}".into())
        }
        NeedsReviewItem::Relation { raw, .. } => {
            serde_json::to_string(raw).unwrap_or_else(|_| "{}".into())
        }
        NeedsReviewItem::Fact { raw, .. } => {
            serde_json::to_string(raw).unwrap_or_else(|_| "{}".into())
        }
    };
    format!(
        "You are arbitrating a contradicting or invalid {kind} primitive in an agent memory store.\n\
         Reason flagged: {reason}\n\
         Item JSON: {item_json}\n\n\
         Reply with a JSON object naming the winning and losing primitive ulids, e.g.\n\
         {{\"winner_id\":\"01HX...\",\"loser_id\":\"01HY...\",\"reason\":\"<short explanation>\"}}\n\
         If you cannot decide, reply with an empty JSON object {{}}."
    )
}

/// Best-effort JSON parse of the model's decoded output — shared by the
/// candle, ollama, and cloud-api backends. Tolerant of leading + trailing
/// junk; extracts the first `{` ... matching `}` substring. On any failure
/// (no JSON, bad shape, missing/invalid ulid), returns
/// [`VerifyDecision::deferred()`] so the worker treats it as abstain.
#[doc(hidden)]
#[allow(dead_code)]
pub(crate) fn parse_decision_json(decoded: &str, backend: VerifierBackend) -> VerifyDecision {
    let Some(start) = decoded.find('{') else {
        return VerifyDecision::deferred();
    };
    let bytes = decoded.as_bytes();
    let mut depth = 0_i32;
    let mut end_excl = start;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end_excl = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end_excl == start {
        return VerifyDecision::deferred();
    }
    let slice = &decoded[start..end_excl];
    match serde_json::from_str::<DecisionJson>(slice) {
        Ok(parsed) => parsed.into_decision(backend),
        Err(e) => {
            tracing::warn!(err = %e, "verifier decision post-hoc parse failed; emitting deferred");
            VerifyDecision::deferred()
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct DecisionJson {
    winner_id: Option<String>,
    loser_id: Option<String>,
    reason: Option<String>,
}

#[allow(dead_code)]
impl DecisionJson {
    fn into_decision(self, backend: VerifierBackend) -> VerifyDecision {
        let (Some(winner), Some(loser)) = (
            self.winner_id.as_deref().and_then(|s| ulid::Ulid::from_string(s).ok()),
            self.loser_id.as_deref().and_then(|s| ulid::Ulid::from_string(s).ok()),
        ) else {
            return VerifyDecision::deferred();
        };
        VerifyDecision::arbitrate(
            winner,
            loser,
            self.reason.unwrap_or_else(|| "no reason provided".to_string()),
            backend,
        )
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
