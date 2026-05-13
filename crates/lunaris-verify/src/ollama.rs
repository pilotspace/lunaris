//! [`OllamaVerifier`] — Verifier backed by an Ollama HTTP endpoint.
//!
//! ## Phase 12b — thin wrapper
//!
//! This file was migrated from a direct `/api/chat` HTTP client to a thin
//! wrapper around `Arc<dyn lunaris_llm::LlmBackend>` (`OllamaBackend`) +
//! `LlmVerifier`. The public API is preserved byte-for-byte.
//!
//! ## Schema constraint note (Phase 12c fix — R1)
//!
//! The legacy `OllamaVerifier` sent a JSON-schema `format` field on every
//! `/api/chat` request, constraining the server's output to the
//! `{winner_id, loser_id, reason}` shape (Ollama structured-output mode).
//! Phase 12b dropped this by passing `SchemaConstraint::None`. Phase 12c
//! (this commit) restores it: `OllamaVerifier::new` sets `schema:
//! Some(decision_schema())` in `LlmVerifierOpts`, which `LlmVerifier::verify`
//! forwards as `SchemaConstraint::JsonSchema` to `OllamaBackend`.
//!
//! ## Failure modes (unchanged)
//!
//! | Condition                                    | Returned error                                                          |
//! |----------------------------------------------|--------------------------------------------------------------------------|
//! | reqwest client build failure (TLS, etc.)     | `LunarisError::Storage(StorageError::Backend("ollama-verify client: ..."))` |
//! | HTTP error (network, timeout, etc.)          | `LunarisError::Storage(StorageError::Backend("ollama-verify: ..."))`     |
//! | non-success HTTP status                      | `LunarisError::Storage(StorageError::Backend("ollama-verify: HTTP <status>"))` |
//! | response body fails JSON parse               | `Ok(VerifyDecision::deferred())` — the worker treats parse failure as abstain (not an error) |

use async_trait::async_trait;
use lunaris_core::LunarisError;
use lunaris_llm::{LlmBackend, OllamaBackend, OllamaBackendOpts};
use std::sync::Arc;

use crate::Verifier;
use crate::llm_verifier::{LlmVerifier, LlmVerifierOpts};
use crate::types::{VerifierBackend, VerifyDecision};
use lunaris_extract::NeedsReviewItem;

/// Per-call HTTP timeout (CLAUDE.md "design for failure"). Ollama is local-
/// network but 27B inference is slow; 60s gives the model room to answer.
///
/// Note: `LlmVerifier` applies this as `timeout_ms` in `LlmVerifierOpts`.
const HTTP_TIMEOUT_MS: u64 = 60_000;

/// Default Ollama endpoint.
const DEFAULT_ENDPOINT: &str = "http://localhost:11434";

/// Default Ollama model identifier (matches `ollama pull gemma3:27b`).
pub const DEFAULT_MODEL: &str = "gemma3:27b";

/// Env var override for the model identifier.
pub const ENV_MODEL_OVERRIDE: &str = "OLLAMA_VERIFY_MODEL";

/// Build the canonical arbitration JSON schema for `{winner_id, loser_id, reason}`.
/// This is the same schema the legacy `OllamaVerifier` inlined in the `/api/chat`
/// request body. Passing it as `SchemaConstraint::JsonSchema` via `LlmVerifierOpts`
/// restores Ollama's server-side structured-output enforcement (R1).
fn decision_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "winner_id": { "type": "string" },
            "loser_id":  { "type": "string" },
            "reason":    { "type": "string" }
        },
        "required": ["winner_id", "loser_id", "reason"]
    })
}

/// Construction options for [`OllamaVerifier`].
///
/// `Default` resolves `endpoint` from `OLLAMA_URL` env (falls back to
/// `http://localhost:11434`) and `model` from `OLLAMA_VERIFY_MODEL` env
/// (falls back to `gemma3:27b`).
#[derive(Clone, Debug)]
pub struct OllamaVerifierOpts {
    pub endpoint: String,
    pub model: String,
}

impl Default for OllamaVerifierOpts {
    fn default() -> Self {
        Self {
            endpoint: std::env::var("OLLAMA_URL").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string()),
            model: std::env::var(ENV_MODEL_OVERRIDE).unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
        }
    }
}

/// Ollama-backed verifier. Phase 12b wraps `OllamaBackend` + `LlmVerifier`.
///
/// Construction is sync (unchanged from legacy — `OllamaBackend::new` is
/// sync). The public surface is identical to Phase 4.
#[derive(Clone)]
pub struct OllamaVerifier {
    inner: LlmVerifier,
}

impl std::fmt::Debug for OllamaVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaVerifier")
            .field("inner", &self.inner)
            .finish()
    }
}

impl OllamaVerifier {
    pub fn new(opts: OllamaVerifierOpts) -> Result<Self, LunarisError> {
        let backend_opts = OllamaBackendOpts { endpoint: opts.endpoint, model: opts.model };
        let backend: Arc<dyn LlmBackend> = Arc::new(OllamaBackend::new(backend_opts)?);
        let verifier = LlmVerifier::with_opts(
            backend,
            LlmVerifierOpts {
                timeout_ms: HTTP_TIMEOUT_MS,
                max_tokens: 1024,
                temperature: 0.0,
                backend_tag: VerifierBackend::Ollama,
                // R1: restore server-side JSON-schema enforcement so Ollama
                // constrains its output to {winner_id, loser_id, reason}.
                schema: Some(decision_schema()),
                // R2: restore pre-Phase-12 nack-on-transport-failure signal.
                // When Ollama returns a transport error, the worker sees Err
                // and nacks the message for redelivery (D-06 contract).
                propagate_transport_errors: true,
                ..LlmVerifierOpts::default()
            },
        );
        Ok(Self { inner: verifier })
    }
}

#[async_trait]
impl Verifier for OllamaVerifier {
    async fn verify(&self, item: NeedsReviewItem) -> Result<VerifyDecision, LunarisError> {
        self.inner.verify(item).await
    }

    fn applies(&self) -> bool {
        self.inner.applies()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opts_default_model_is_gemma3_27b() {
        // Guard the env-var reader from the test-runner shell — only check
        // that defaults resolve to non-empty strings AND the DEFAULT_MODEL
        // constant is the 27B variant.
        let opts = OllamaVerifierOpts::default();
        assert!(!opts.endpoint.is_empty());
        assert!(!opts.model.is_empty());
        assert_eq!(DEFAULT_MODEL, "gemma3:27b");
        assert_eq!(ENV_MODEL_OVERRIDE, "OLLAMA_VERIFY_MODEL");
    }

    #[test]
    fn verifier_construction_succeeds_with_defaults() {
        let _v = OllamaVerifier::new(OllamaVerifierOpts::default()).expect("client builds");
    }

    // ── R2 regression: transport errors propagate as Err for worker nack ──────
    /// When `OllamaBackend` returns a transport error, `OllamaVerifier::verify`
    /// must surface it as `Err` so the worker nacks the message and the broker
    /// redelivers. Pre-Phase-12 `OllamaVerifier` returned `Err` on every HTTP /
    /// network failure; Phase 12b silently swallowed it as `Ok(deferred())`.
    #[tokio::test]
    async fn ollama_transport_error_propagates_for_worker_nack() {
        use std::sync::Arc;

        use async_trait::async_trait;
        use lunaris_core::{LunarisError, StorageError};
        use lunaris_extract::{EntityId, Fact, NeedsReviewItem, NeedsReviewReason};
        use lunaris_llm::{GenOpts, LlmBackend, SchemaConstraint};
        use ulid::Ulid;

        use crate::llm_verifier::{LlmVerifier, LlmVerifierOpts};
        use crate::types::VerifierBackend;
        use crate::Verifier;

        struct TransportErrorBackend;
        #[async_trait]
        impl LlmBackend for TransportErrorBackend {
            async fn generate(
                &self,
                _: &str,
                _: SchemaConstraint<'_>,
                _: GenOpts,
            ) -> Result<String, LunarisError> {
                Err(LunarisError::Storage(StorageError::Backend(
                    "ollama: connection refused".into(),
                )))
            }
            fn model_id(&self) -> &str {
                "stub://ollama-transport-err"
            }
        }

        let verifier = LlmVerifier::with_opts(
            Arc::new(TransportErrorBackend),
            LlmVerifierOpts {
                timeout_ms: HTTP_TIMEOUT_MS,
                max_tokens: 1024,
                temperature: 0.0,
                backend_tag: VerifierBackend::Ollama,
                schema: Some(decision_schema()),
                propagate_transport_errors: true,
                ..LlmVerifierOpts::default()
            },
        );

        let sid = EntityId::from_name_and_type("Alice", "Person");
        let oid = EntityId::from_name_and_type("Bob", "Person");
        let item = NeedsReviewItem::Fact {
            reason: NeedsReviewReason::GbnfFailure {
                schema_path: "facts/0".into(),
                error: "fixture".into(),
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
        };

        let result: Result<_, lunaris_core::LunarisError> = verifier.verify(item).await;
        assert!(
            result.is_err(),
            "transport error must propagate as Err so worker nacks the message"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("connection refused"),
            "error must include the transport cause: {msg}"
        );
    }

    // ── R1 regression: decision_schema contains all required arbitration fields ─
    /// `decision_schema()` must produce a JSON schema that names all three
    /// required fields (`winner_id`, `loser_id`, `reason`) so Ollama's
    /// structured-output mode constrains its response to the arbitration shape.
    /// This restores the server-side enforcement dropped in Phase 12b.
    #[test]
    fn ollama_decision_schema_has_required_arbitration_fields() {
        let schema = decision_schema();
        let props = schema["properties"].as_object().expect("schema must have properties");
        assert!(props.contains_key("winner_id"), "schema must require winner_id");
        assert!(props.contains_key("loser_id"), "schema must require loser_id");
        assert!(props.contains_key("reason"), "schema must require reason");
        let required = schema["required"].as_array().expect("schema must have required array");
        let required_names: Vec<&str> =
            required.iter().filter_map(|v| v.as_str()).collect();
        assert!(required_names.contains(&"winner_id"), "winner_id must be required");
        assert!(required_names.contains(&"loser_id"), "loser_id must be required");
        assert!(required_names.contains(&"reason"), "reason must be required");
    }
}
