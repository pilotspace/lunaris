//! [`OllamaVerifier`] — Verifier backed by an Ollama HTTP endpoint.
//!
//! ## Phase 12b — thin wrapper
//!
//! This file was migrated from a direct `/api/chat` HTTP client to a thin
//! wrapper around `Arc<dyn lunaris_llm::LlmBackend>` (`OllamaBackend`) +
//! `LlmVerifier`. The public API is preserved byte-for-byte.
//!
//! ## Schema constraint note (Phase 12b behavioral change)
//!
//! The legacy `OllamaVerifier` sent a JSON-schema `format` field on every
//! `/api/chat` request, constraining the server's output to the
//! `{winner_id, loser_id, reason}` shape (Ollama structured-output mode).
//! `LlmVerifier` delegates via `SchemaConstraint::None` — no `format` field
//! is sent. This is a known behavioral delta: server-side schema enforcement
//! is dropped. The post-hoc `parse_decision_json` parse still runs on the
//! response, and malformed output returns `VerifyDecision::deferred()`.
//! Impact: models that need the JSON-schema hint to stay on format will
//! drift toward deferred decisions more often. A follow-up can wire
//! `SchemaConstraint::JsonSchema` through `LlmVerifier` when that feature
//! lands. Tracked: Phase 12c (schema-constraint propagation).
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
}
