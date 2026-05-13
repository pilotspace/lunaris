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
    /// Optional JSON schema to pass as [`SchemaConstraint::JsonSchema`] on
    /// every `backend.generate()` call. When `None` (default), falls back
    /// to [`SchemaConstraint::None`] — free-form generation + post-hoc
    /// parse. Ollama wires the `{winner_id, loser_id, reason}` schema here
    /// (R1); cloud-API wires the same schema so provider-native structured
    /// output is used (R4). Additive, non-breaking — `Default` is `None`.
    pub schema: Option<serde_json::Value>,
    /// When `true`, backend errors are returned as
    /// `Ok(VerifyDecision::deferred_with_cause(...))` with the error message
    /// embedded in `cause` and `backend_tag` preserved. Default is `false`
    /// (plain `deferred()`). Cloud-api sets this `true` to restore the D-21
    /// audit-reason signal lost in Phase 12b (R3).
    pub defer_with_cause_on_error: bool,
}

impl Default for LlmVerifierOpts {
    fn default() -> Self {
        Self {
            timeout_ms: 1500,
            max_tokens: 256,
            temperature: 0.0,
            backend_tag: VerifierBackend::Candle,
            schema: None,
            defer_with_cause_on_error: false,
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
        let constraint = match self.opts.schema.as_ref() {
            Some(schema) => SchemaConstraint::JsonSchema(schema),
            None => SchemaConstraint::None,
        };
        match self.backend.generate(&prompt, constraint, gen_opts).await {
            Ok(decoded) => Ok(parse_decision_json(&decoded, self.opts.backend_tag)),
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    model_id = self.backend.model_id(),
                    "LlmVerifier generate failed; emitting deferred decision"
                );
                if self.opts.defer_with_cause_on_error {
                    Ok(VerifyDecision::deferred_with_cause(
                        format!("transient_after_retry: {e}"),
                        self.opts.backend_tag,
                    ))
                } else {
                    Ok(VerifyDecision::deferred())
                }
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

    // ── R1 regression: schema opt is forwarded to backend as JsonSchema ───────
    /// Verifies that when `LlmVerifierOpts.schema` is set, the backend receives
    /// `SchemaConstraint::JsonSchema` rather than `SchemaConstraint::None`.
    /// This is the unit contract for R1 (Ollama) and R4 (cloud-api) — the
    /// wrappers set the schema; this test proves `LlmVerifier` plumbs it.
    #[tokio::test]
    async fn schema_opt_forwarded_to_backend_as_json_schema() {
        use std::sync::{Arc, Mutex};
        struct SchemaCapturingBackend {
            received_json_schema: Arc<Mutex<bool>>,
        }
        #[async_trait]
        impl LlmBackend for SchemaCapturingBackend {
            async fn generate(
                &self,
                _prompt: &str,
                constraint: SchemaConstraint<'_>,
                _opts: GenOpts,
            ) -> Result<String, LunarisError> {
                if matches!(constraint, SchemaConstraint::JsonSchema(_)) {
                    *self.received_json_schema.lock().unwrap() = true;
                }
                Ok("{}".into()) // returns empty JSON → deferred, but that's fine
            }
            fn model_id(&self) -> &str {
                "stub://schema-test"
            }
        }

        let flag = Arc::new(Mutex::new(false));
        let backend: Arc<dyn LlmBackend> =
            Arc::new(SchemaCapturingBackend { received_json_schema: flag.clone() });
        let schema = serde_json::json!({"type": "object", "properties": {"winner_id": {"type": "string"}}});
        let verifier = LlmVerifier::with_opts(
            backend,
            LlmVerifierOpts { schema: Some(schema), ..LlmVerifierOpts::default() },
        );
        let _ = verifier.verify(fact_item()).await.unwrap();
        assert!(
            *flag.lock().unwrap(),
            "backend must receive SchemaConstraint::JsonSchema when opts.schema is set"
        );
    }

    // ── R3 regression: deferred_with_cause carries reason + backend tag ───────
    /// When `defer_with_cause_on_error = true`, a backend `Err` must return
    /// `Ok(deferred_with_cause(...))` so the D-22 audit event carries the
    /// `transient_after_retry` signal and the correct provider backend tag.
    #[tokio::test]
    async fn backend_error_deferred_with_cause_preserves_audit_reason() {
        use lunaris_core::StorageError;
        struct TransientBackend;
        #[async_trait]
        impl LlmBackend for TransientBackend {
            async fn generate(
                &self,
                _: &str,
                _: SchemaConstraint<'_>,
                _: GenOpts,
            ) -> Result<String, LunarisError> {
                Err(LunarisError::Storage(StorageError::Backend(
                    "cloud-api: HTTP 503: service unavailable".into(),
                )))
            }
            fn model_id(&self) -> &str {
                "stub://transient"
            }
        }
        let verifier = LlmVerifier::with_opts(
            Arc::new(TransientBackend),
            LlmVerifierOpts {
                backend_tag: VerifierBackend::CloudAnthropic,
                defer_with_cause_on_error: true,
                ..LlmVerifierOpts::default()
            },
        );
        let decision = verifier.verify(fact_item()).await.expect("must return Ok(deferred)");
        assert!(!decision.applies(), "deferred_with_cause must not apply");
        assert_eq!(
            decision.backend,
            VerifierBackend::CloudAnthropic,
            "backend tag must be preserved for D-22 audit"
        );
        let cause = decision.cause.expect("cause must be Some for transient_after_retry audit");
        assert!(
            cause.contains("transient_after_retry"),
            "cause must contain transient_after_retry prefix: {cause}"
        );
        assert!(
            cause.contains("HTTP 503"),
            "cause must include the upstream error detail: {cause}"
        );
    }
}
