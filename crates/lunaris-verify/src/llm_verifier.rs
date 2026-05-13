//! [`LlmVerifier`] — backend-agnostic [`Verifier`] consuming any
//! `Arc<dyn LlmBackend>` from `lunaris-llm`.
//!
//! ## Why this exists
//!
//! Phase 11 unifies the three LLM-using pipelines (extract, verify,
//! reflect) on `lunaris_llm::LlmBackend`. This adapter is the
//! verify-side seat: it owns the arbitration-prompt template and the
//! `{winner_id, loser_id, reason}` JSON post-hoc parse. Whatever
//! `LlmBackend` impl the caller picks (`CandleBackend` 4B/27B/270M,
//! `OllamaBackend`, `CloudBackend`) flows through this one verifier.
//!
//! ## Verify default is still 27B
//!
//! This commit is **additive**: legacy [`crate::CandleGemma3_27B`],
//! [`crate::OllamaVerifier`], and [`crate::CloudApiVerifier`] are
//! unchanged. The umbrella `Lunaris::with_verifier` default is
//! [`crate::NoopVerifier`] (blueprint §5.1) and any consumer that
//! explicitly enables verify today gets the same 27B path it had
//! before. The flip to 4B default is gated on the ER-F1 quality test
//! (Task 6 in the migration plan) — landed in a separate commit that
//! cites the gate evidence.
//!
//! ## Per-call timeout
//!
//! 27B is ~7-10× slower than 4B for the same prompt. The default
//! [`LlmVerifierOpts::timeout_ms`] is 1500 ms to give the slow path
//! headroom; callers using a 4B backend should override downward to
//! match the D-02 budget (≈ 200 ms).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::LunarisError;
use lunaris_llm::{GenOpts, LlmBackend, SchemaConstraint};

use crate::types::VerifierBackend;
use crate::{Verifier, VerifyDecision, arbitration_prompt, parse_decision_json};
use lunaris_extract::NeedsReviewItem;

/// Construction options for [`LlmVerifier`].
#[derive(Clone, Debug)]
pub struct LlmVerifierOpts {
    /// Per-call timeout. Defaults to 1500 ms to accommodate 27B; lower
    /// to ~200 ms when the backend is 4B / 270M.
    pub timeout_ms: u64,
    /// Max output tokens. Arbitration decisions are short — 256 covers
    /// `{winner_id, loser_id, reason}` plus a ~200-char reason.
    pub max_tokens: u32,
    /// Sampling temperature. 0.0 = greedy (recommended for arbitration).
    pub temperature: f32,
    /// Backend tag stamped into the returned [`VerifyDecision`] for
    /// audit/telemetry. Defaults to [`VerifierBackend::Candle`]; ollama/
    /// cloud-api consumers should override.
    pub backend_tag: VerifierBackend,
}

impl Default for LlmVerifierOpts {
    fn default() -> Self {
        Self {
            timeout_ms: 1500,
            max_tokens: 256,
            temperature: 0.0,
            backend_tag: VerifierBackend::Candle,
        }
    }
}

/// Backend-agnostic verifier. Holds an `Arc<dyn LlmBackend>` so the
/// underlying provider can be swapped at runtime via `LlmConfig`.
#[derive(Clone)]
pub struct LlmVerifier {
    backend: Arc<dyn LlmBackend>,
    opts: LlmVerifierOpts,
}

impl std::fmt::Debug for LlmVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmVerifier")
            .field("model_id", &self.backend.model_id())
            .field("opts", &self.opts)
            .finish()
    }
}

impl LlmVerifier {
    pub fn new(backend: Arc<dyn LlmBackend>) -> Self {
        Self { backend, opts: LlmVerifierOpts::default() }
    }

    pub fn with_opts(backend: Arc<dyn LlmBackend>, opts: LlmVerifierOpts) -> Self {
        Self { backend, opts }
    }
}

#[async_trait]
impl Verifier for LlmVerifier {
    async fn verify(&self, item: NeedsReviewItem) -> Result<VerifyDecision, LunarisError> {
        let prompt = arbitration_prompt(&item);
        let gen_opts = GenOpts {
            max_tokens: self.opts.max_tokens,
            temperature: self.opts.temperature,
            timeout: Duration::from_millis(self.opts.timeout_ms),
        };
        match self.backend.generate(&prompt, SchemaConstraint::None, gen_opts).await {
            Ok(decoded) => Ok(parse_decision_json(&decoded, self.opts.backend_tag)),
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    model_id = self.backend.model_id(),
                    "LlmVerifier generate failed; emitting deferred decision"
                );
                Ok(VerifyDecision::deferred())
            }
        }
    }

    fn applies(&self) -> bool {
        self.backend.applies()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lunaris_extract::{EntityId, Fact, NeedsReviewReason};
    use ulid::Ulid;

    struct StubBackend {
        out: String,
        delay: Duration,
    }

    #[async_trait]
    impl LlmBackend for StubBackend {
        async fn generate(
            &self,
            _prompt: &str,
            _constraint: SchemaConstraint<'_>,
            _opts: GenOpts,
        ) -> Result<String, LunarisError> {
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            Ok(self.out.clone())
        }
        fn model_id(&self) -> &str {
            "stub://verify-test"
        }
    }

    fn fact_item() -> NeedsReviewItem {
        let sid = EntityId::from_name_and_type("Alice", "Person");
        let oid = EntityId::from_name_and_type("Bob", "Person");
        NeedsReviewItem::Fact {
            reason: NeedsReviewReason::GbnfFailure {
                schema_path: "facts/0".into(),
                error: "test fixture".into(),
            },
            raw: Fact {
                id: Ulid::new(),
                subject_id: sid,
                predicate: "knows".into(),
                object_id: oid,
                fact_text: "Alice knows Bob".into(),
                confidence: 0.9,
                valid_from_iso: "2025-01-01".into(),
                valid_to_iso: None,
            },
        }
    }

    #[tokio::test]
    async fn parses_arbitration_json() {
        let winner = Ulid::new();
        let loser = Ulid::new();
        let out = format!(
            r#"{{"winner_id":"{winner}","loser_id":"{loser}","reason":"newer fact wins"}}"#
        );
        let backend: Arc<dyn LlmBackend> = Arc::new(StubBackend { out, delay: Duration::ZERO });
        let verifier = LlmVerifier::new(backend);
        let decision = verifier.verify(fact_item()).await.unwrap();
        assert!(decision.applies());
    }

    #[tokio::test]
    async fn malformed_output_returns_deferred() {
        let backend: Arc<dyn LlmBackend> =
            Arc::new(StubBackend { out: "totally not JSON".into(), delay: Duration::ZERO });
        let verifier = LlmVerifier::new(backend);
        let decision = verifier.verify(fact_item()).await.unwrap();
        assert!(!decision.applies());
    }

    #[tokio::test]
    async fn applies_reflects_backend() {
        struct NoopBackend;
        #[async_trait]
        impl LlmBackend for NoopBackend {
            async fn generate(
                &self,
                _: &str,
                _: SchemaConstraint<'_>,
                _: GenOpts,
            ) -> Result<String, LunarisError> {
                Ok(String::new())
            }
            fn model_id(&self) -> &str {
                "stub://noop"
            }
            fn applies(&self) -> bool {
                false
            }
        }
        let v = LlmVerifier::new(Arc::new(NoopBackend));
        assert!(!v.applies());
    }
}
