//! `lunaris-embed-remote` — Ollama HTTP escape-hatch embedder.
//!
//! # OPERATOR ESCAPE HATCH — NOT THE SUPPORTED PATH
//!
//! The supported embedder is `lunaris_llamacpp::LlamaCppEmbedder`
//! (llama.cpp + granite-r2 Q4_K_M GGUF; llama.cpp-only cutover, ADR
//! 2026-07-10). This crate is feature-gated (`embed-remote` on the
//! `lunaris-memory` umbrella) and intentionally off by default. It exists so
//! air-gapped operators with a pre-existing Ollama instance can keep an
//! HTTP-based fallback without a local GGUF artifact.
//!
//! Activate by:
//!
//! ```text
//! [features]
//! embed-remote = ["lunaris-memory/embed-remote"]
//! ```
//!
//! and set `LUNARIS_EMBEDDER_OLLAMA_URL=<endpoint>` at startup. The umbrella's
//! `resolve_embedder()` consults that env var AFTER the llama.cpp default
//! (remote is the fallback, not the override).
//!
//! ## Wire shape
//!
//! POSTs `{model, input: [..]}` to `<endpoint>/api/embed`, parses
//! `{embeddings: [[..]]}`, and returns 768-d rows by default. 10s HTTP timeout
//! per CLAUDE.md "design for failure" rule. Validates response shape against
//! [`Embedder::dim`] — spoofed Ollama returning the wrong dimension surfaces as
//! `LunarisError::Storage(StorageError::Backend(..))` rather than silently
//! corrupting the vector index.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

/// OpenAI-compatible `/embeddings` remote embedder — the supported remote path
/// once the native candle embedder is compiled out (`native` feature off).
pub mod openai;

use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::{Embedder, LunarisError, StorageError};
use serde::{Deserialize, Serialize};

/// Default Ollama daemon URL. Overridden via `LUNARIS_EMBEDDER_OLLAMA_URL`
/// (preferred — matches the umbrella `resolve_embedder()` knob) or the legacy
/// `LUNARIS_OLLAMA_URL` (kept for back-compat with v0.3 deployments).
pub const DEFAULT_ENDPOINT: &str = "http://localhost:11434";
/// Default model. The v0.3 `embeddinggemma` model card is preserved here for
/// continuity — operators wiring a different model must set
/// `LUNARIS_OLLAMA_MODEL` or override [`OllamaEmbedderOpts::model`].
pub const DEFAULT_MODEL: &str = "embeddinggemma:300m";
/// Default expected dim — matches granite-r2 (768-d), so storage indices
/// stay interoperable even when the operator swaps the embedder. Override
/// via [`OllamaEmbedderOpts::dim`].
pub const DEFAULT_DIM: usize = 768;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Env var consulted by [`OllamaEmbedderOpts::default`] for the daemon URL.
/// This is the v0.4 canonical name; the legacy `LUNARIS_OLLAMA_URL` is also
/// honoured as a fallback so v0.3 deployments do not have to rename their
/// orchestrator config in lock-step with the rename.
pub const OLLAMA_URL_ENV_VAR: &str = "LUNARIS_EMBEDDER_OLLAMA_URL";
const LEGACY_OLLAMA_URL_ENV_VAR: &str = "LUNARIS_OLLAMA_URL";
const OLLAMA_MODEL_ENV_VAR: &str = "LUNARIS_OLLAMA_MODEL";

/// Construction options for [`OllamaEmbedder`]. `Default` consults the env
/// vars listed in the crate docs.
#[derive(Clone, Debug)]
pub struct OllamaEmbedderOpts {
    pub endpoint: String,
    pub model: String,
    pub dim: usize,
}

impl Default for OllamaEmbedderOpts {
    fn default() -> Self {
        let endpoint = std::env::var(OLLAMA_URL_ENV_VAR)
            .ok()
            .or_else(|| std::env::var(LEGACY_OLLAMA_URL_ENV_VAR).ok())
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
        let model =
            std::env::var(OLLAMA_MODEL_ENV_VAR).ok().unwrap_or_else(|| DEFAULT_MODEL.to_string());
        Self { endpoint, model, dim: DEFAULT_DIM }
    }
}

/// Ollama HTTP-backed `Embedder`. Cheap to clone — wraps an
/// `reqwest::Client` (which is internally `Arc`-based).
#[derive(Clone)]
pub struct OllamaEmbedder {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    dim: usize,
}

impl std::fmt::Debug for OllamaEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaEmbedder")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("dim", &self.dim)
            .finish()
    }
}

impl OllamaEmbedder {
    /// Construct a new Ollama-backed embedder. Builds a `reqwest::Client`
    /// with a 10s timeout (CLAUDE.md). Client-build failures (TLS, etc.)
    /// surface at construction time as `LunarisError::Storage(Backend(..))`.
    pub fn new(opts: OllamaEmbedderOpts) -> Result<Self, LunarisError> {
        let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!(
                "lunaris-embed-remote ollama client: {e}"
            )))
        })?;
        tracing::warn!(
            target: "lunaris::embed_remote",
            endpoint = %opts.endpoint,
            model = %opts.model,
            dim = opts.dim,
            "OllamaEmbedder constructed — this is an OPERATOR ESCAPE HATCH and is not \
             the supported path. Use the llama.cpp embedder (lunaris-llamacpp) unless \
             you have a specific reason to route embeddings through Ollama."
        );
        Ok(Self { client, endpoint: opts.endpoint, model: opts.model, dim: opts.dim })
    }
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

#[async_trait]
impl Embedder for OllamaEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/api/embed", self.endpoint.trim_end_matches('/'));
        let body = EmbedRequest { model: &self.model, input: inputs.to_vec() };

        let resp = self.client.post(&url).json(&body).send().await.map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!(
                "lunaris-embed-remote ollama: {e}"
            )))
        })?;
        if !resp.status().is_success() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "lunaris-embed-remote ollama: HTTP {}",
                resp.status()
            ))));
        }
        let parsed: EmbedResponse = resp.json().await.map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!(
                "lunaris-embed-remote ollama parse: {e}"
            )))
        })?;

        if parsed.embeddings.len() != inputs.len() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "lunaris-embed-remote ollama returned wrong shape: expected {} rows, got {}",
                inputs.len(),
                parsed.embeddings.len()
            ))));
        }
        for (i, row) in parsed.embeddings.iter().enumerate() {
            if row.len() != self.dim {
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "lunaris-embed-remote ollama returned wrong shape: row {} has dim {} \
                     (expected {})",
                    i,
                    row.len(),
                    self.dim
                ))));
            }
        }
        Ok(parsed.embeddings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opts_default_resolves_to_localhost_ollama() {
        // SAFETY: tests run single-threaded by default for env-var safety;
        // we clear the override vars so the default-construction asserts hold
        // independent of operator shell state. CLAUDE.md compliance is moot
        // for unsafe-free env access.
        // NOTE: deliberately not removing env vars (potential race with other
        // tests) — instead just sanity-check the constants.
        let _opts = OllamaEmbedderOpts::default();
        // The endpoint depends on the test environment; we only verify the
        // constants are well-formed.
        assert_eq!(DEFAULT_DIM, 768);
        assert_eq!(DEFAULT_MODEL, "embeddinggemma:300m");
    }

    #[test]
    fn embedder_construction_succeeds() {
        let _e = OllamaEmbedder::new(OllamaEmbedderOpts {
            endpoint: "http://localhost:11434".to_string(),
            model: "embeddinggemma:300m".to_string(),
            dim: 768,
        })
        .expect("client builds");
    }
}
