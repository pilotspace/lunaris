//! Ollama HTTP [`LlmBackend`] — POSTs `/api/chat` with optional JSON-schema
//! `format` field.
//!
//! ## Scope
//!
//! Generic wire-level Ollama client. Caller provides the prompt and an
//! optional [`SchemaConstraint`]; this module handles only the HTTP +
//! parse-text-out half. Domain-specific behaviour (entity extraction
//! prompt assembly, arbitration JSON parsing) stays in `lunaris-extract`
//! / `lunaris-verify`.
//!
//! ## Constraint handling
//!
//! - [`SchemaConstraint::None`] — no `format` field on the request.
//! - [`SchemaConstraint::JsonSchema`] — passed through verbatim as
//!   the `format` field. Ollama (0.5+) enforces structured output
//!   server-side, matching the existing extract/verify behavior.
//! - [`SchemaConstraint::Gbnf`] — Ollama does NOT consume GBNF; the
//!   grammar is dropped and the caller is responsible for embedding it
//!   in the prompt (existing `lunaris-extract::OllamaExtractor` already
//!   does this — the grammar is appended to the user message).
//!
//! ## Timeouts
//!
//! Two layers, matching the existing extract pattern:
//!
//! - `reqwest::Client` is built with a fixed 10s timeout — protects
//!   against TCP-level hangs.
//! - [`GenOpts::timeout`] wraps the outer call via
//!   `tokio::time::timeout` — clamps per-call latency to the D-02 budget.

use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::{LunarisError, StorageError};
use serde::{Deserialize, Serialize};

use crate::{GenOpts, LlmBackend, SchemaConstraint};

/// Per-call HTTP transport timeout. The narrower [`GenOpts::timeout`]
/// clamps further on top of this.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Default Ollama endpoint when none is supplied.
const DEFAULT_ENDPOINT: &str = "http://localhost:11434";

/// Construction options for [`OllamaBackend`].
#[derive(Clone, Debug)]
pub struct OllamaBackendOpts {
    /// Base URL (no trailing slash needed). Defaults to `OLLAMA_URL` env,
    /// then `http://localhost:11434`.
    pub endpoint: String,
    /// Ollama model identifier (e.g. `"gemma3:4b"`, `"llama3.2:3b"`).
    pub model: String,
}

impl OllamaBackendOpts {
    /// Construct from env. `endpoint` reads `OLLAMA_URL`; `model` must be
    /// supplied (no implicit default — caller has the model name from
    /// `LlmConfig`).
    pub fn from_env(model: impl Into<String>) -> Self {
        Self {
            endpoint: std::env::var("OLLAMA_URL").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string()),
            model: model.into(),
        }
    }
}

/// HTTP-backed LLM backend speaking the Ollama `/api/chat` protocol.
#[derive(Clone, Debug)]
pub struct OllamaBackend {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    model_id: String,
}

impl OllamaBackend {
    pub fn new(opts: OllamaBackendOpts) -> Result<Self, LunarisError> {
        let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("ollama client: {e}")))
        })?;
        let model_id = format!("ollama://{}", opts.model);
        Ok(Self { client, endpoint: opts.endpoint, model: opts.model, model_id })
    }
}

#[async_trait]
impl LlmBackend for OllamaBackend {
    async fn generate(
        &self,
        prompt: &str,
        constraint: SchemaConstraint<'_>,
        opts: GenOpts,
    ) -> Result<String, LunarisError> {
        let url = format!("{}/api/chat", self.endpoint.trim_end_matches('/'));

        // Convert constraint → wire-level `format` field. Ollama's GBNF
        // support is non-existent (as of 0.5); GBNF flavours fall through
        // to None + caller-side prompt embedding.
        let format = match constraint {
            SchemaConstraint::None | SchemaConstraint::Gbnf(_) => None,
            SchemaConstraint::JsonSchema(schema) => Some(schema.clone()),
        };

        let request = ChatRequest {
            model: &self.model,
            messages: &[ChatMessage { role: "user", content: prompt }],
            format,
            stream: false,
            options: Some(GenerationOptions {
                temperature: opts.temperature,
                num_predict: opts.max_tokens as i32,
            }),
        };

        let send = async {
            let resp = self.client.post(&url).json(&request).send().await.map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!("ollama: {e}")))
            })?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "ollama: HTTP {status}: {body}"
                ))));
            }
            let parsed: ChatResponse = resp.json().await.map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!("ollama parse: {e}")))
            })?;
            Ok::<_, LunarisError>(parsed.message.content)
        };

        match tokio::time::timeout(opts.timeout, send).await {
            Ok(r) => r,
            Err(_) => Err(LunarisError::Storage(StorageError::Backend(format!(
                "ollama timeout after {:?}",
                opts.timeout
            )))),
        }
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage<'a>],
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<serde_json::Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<GenerationOptions>,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct GenerationOptions {
    temperature: f32,
    num_predict: i32,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    message: ChatResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ChatResponseMessage {
    content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env-reading tests are intentionally omitted — Rust 2024's
    // `std::env::set_var` is unsafe, and this crate carries
    // `#![forbid(unsafe_code)]`. The `OllamaBackendOpts::from_env` path
    // is covered indirectly by integration tests in the consuming crates
    // (which can set env in a controlled test harness with an unsafe
    // scope that this leaf crate refuses to allow).

    #[test]
    fn model_id_is_namespaced() {
        let backend = OllamaBackend::new(OllamaBackendOpts {
            endpoint: "http://localhost:11434".into(),
            model: "gemma3:4b".into(),
        })
        .unwrap();
        assert_eq!(backend.model_id(), "ollama://gemma3:4b");
    }

    #[test]
    fn explicit_opts_preserve_endpoint_and_model() {
        let backend = OllamaBackend::new(OllamaBackendOpts {
            endpoint: "http://example.invalid:9999".into(),
            model: "llama3.2:3b".into(),
        })
        .unwrap();
        assert_eq!(backend.endpoint, "http://example.invalid:9999");
        assert_eq!(backend.model, "llama3.2:3b");
    }
}
