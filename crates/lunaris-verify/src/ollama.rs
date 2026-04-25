//! [`OllamaVerifier`] — Verifier backed by an Ollama HTTP endpoint.
//!
//! POSTs `/api/chat` with `{model, messages, format: <json_schema>, stream:
//! false}` where `format` is the JSON-schema `{winner_id, loser_id, reason}`
//! that constrains the model's output to the arbitration decision shape.
//! 60s HTTP timeout per CLAUDE.md "design for failure" rule (27B is slower
//! than 4B so budget is doubled from lunaris-extract's 30s default).
//!
//! ## Failure modes
//!
//! | Condition                                    | Returned error                                                          |
//! |----------------------------------------------|--------------------------------------------------------------------------|
//! | reqwest client build failure (TLS, etc.)     | `LunarisError::Storage(StorageError::Backend("ollama-verify client: ..."))` |
//! | HTTP error (network, timeout, etc.)          | `LunarisError::Storage(StorageError::Backend("ollama-verify: ..."))`     |
//! | non-success HTTP status                      | `LunarisError::Storage(StorageError::Backend("ollama-verify: HTTP <status>"))` |
//! | response body fails JSON parse               | `Ok(VerifyDecision::deferred())` — the worker treats parse failure as abstain (not an error) |

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::{LunarisError, StorageError};
use serde::{Deserialize, Serialize};

use crate::Verifier;
use crate::parse_decision_json;
use crate::types::{VerifierBackend, VerifyDecision};
use lunaris_extract::NeedsReviewItem;

/// Per-call HTTP timeout (CLAUDE.md "design for failure"). Ollama is local-
/// network but 27B inference is slow; 60s gives the model room to answer.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

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

#[derive(Clone)]
pub struct OllamaVerifier {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    /// Cached JSON-schema for the `{winner_id, loser_id, reason}` arbitration
    /// output. Built once at construction time; sent in every `/api/chat`
    /// request as the `format` field so Ollama enforces the structured
    /// output server-side.
    format_schema: Arc<serde_json::Value>,
}

impl std::fmt::Debug for OllamaVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaVerifier")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .finish()
    }
}

impl OllamaVerifier {
    pub fn new(opts: OllamaVerifierOpts) -> Result<Self, LunarisError> {
        let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("ollama-verify client: {e}")))
        })?;
        Ok(Self {
            client,
            endpoint: opts.endpoint,
            model: opts.model,
            format_schema: Arc::new(default_format_schema()),
        })
    }
}

#[async_trait]
impl Verifier for OllamaVerifier {
    async fn verify(&self, item: NeedsReviewItem) -> Result<VerifyDecision, LunarisError> {
        let url = format!("{}/api/chat", self.endpoint.trim_end_matches('/'));
        let prompt = crate::arbitration_prompt(&item);
        let body = ChatRequest {
            model: self.model.clone(),
            messages: vec![ChatMessage { role: "user".into(), content: prompt }],
            format: (*self.format_schema).clone(),
            stream: false,
        };

        let resp = self.client.post(&url).json(&body).send().await.map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("ollama-verify: {e}")))
        })?;
        if !resp.status().is_success() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "ollama-verify: HTTP {}",
                resp.status()
            ))));
        }

        let parsed: ChatResponse = resp.json().await.map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("ollama-verify parse: {e}")))
        })?;

        // Defer gracefully on parse failure — the worker treats a deferred
        // decision as "skip this item" rather than a hard error.
        Ok(parse_decision_json(&parsed.message.content, VerifierBackend::Ollama))
    }
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    format: serde_json::Value,
    stream: bool,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    message: ChatMessageOut,
}

#[derive(Deserialize)]
struct ChatMessageOut {
    content: String,
}

/// JSON-schema for the decision shape. Matches the parse target of
/// `crate::parse_decision_json` (the shared post-hoc parse helper).
fn default_format_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "winner_id": {"type": "string"},
            "loser_id":  {"type": "string"},
            "reason":    {"type": "string"}
        },
        "required": ["winner_id", "loser_id", "reason"]
    })
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

    #[test]
    fn format_schema_has_required_top_level_keys() {
        let s = default_format_schema();
        assert_eq!(s["type"], "object");
        assert!(s["properties"]["winner_id"].is_object());
        assert!(s["properties"]["loser_id"].is_object());
        assert!(s["properties"]["reason"].is_object());
    }
}
