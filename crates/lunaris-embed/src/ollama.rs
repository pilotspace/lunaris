//! `OllamaEmbedder` — Embedder backed by an Ollama HTTP endpoint.
//!
//! POSTs `{model, input: [..]}` to `<endpoint>/api/embed`, parses
//! `{embeddings: [[..]]}`, and returns 768-d rows. 10s HTTP timeout per
//! CLAUDE.md "design for failure" rule. Validates response shape against
//! [`Embedder::dim`] (T-02-01-04 mitigation: spoofed Ollama returning the
//! wrong dimension surfaces as `LunarisError::Storage(StorageError::Backend)`,
//! not silent corruption of the vector index).
//!
//! This is the **latency-budget escape hatch** the plan specifies: when candle
//! local inference busts the per-batch budget, callers swap to this backend via
//! `Lunaris::with_embedder(Arc::new(OllamaEmbedder::new(opts)))`.

use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::{Embedder, LunarisError, StorageError};
use serde::{Deserialize, Serialize};

const DEFAULT_ENDPOINT: &str = "http://localhost:11434";
const DEFAULT_MODEL: &str = "embeddinggemma:300m";
const DEFAULT_DIM: usize = 768;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Construction options for [`OllamaEmbedder`].
///
/// `Default` resolves to `http://localhost:11434` / `embeddinggemma` / 768d
/// (matches the `ollama pull embeddinggemma` model card).
#[derive(Clone, Debug)]
pub struct OllamaEmbedderOpts {
    pub endpoint: String,
    pub model: String,
    /// Output dimensionality the caller expects (response-shape validator
    /// rejects rows with a different length). Default: 768.
    pub dim: usize,
}

impl Default for OllamaEmbedderOpts {
    fn default() -> Self {
        // Allow tests and benchmarks to redirect the embedder (see
        // `scripts/ollama-replay-server.py`) without rebuilding. Model
        // name is overridable for the same reason — pulling a different
        // variant via `OLLAMA_MODEL` lets ablation runs compare embedders
        // without touching Cargo.toml.
        Self {
            endpoint: std::env::var("LUNARIS_OLLAMA_URL")
                .ok()
                .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
            model: std::env::var("LUNARIS_OLLAMA_MODEL")
                .ok()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            dim: DEFAULT_DIM,
        }
    }
}

#[derive(Clone)]
pub struct OllamaEmbedder {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    dim: usize,
}

impl OllamaEmbedder {
    /// Construct a new Ollama-backed embedder. Builds a `reqwest::Client` with
    /// a 10s timeout (CLAUDE.md). On client-build failure (TLS, etc.) returns
    /// `LunarisError::Storage(StorageError::Backend)` so callers see a
    /// well-shaped error at construction time rather than at first call.
    pub fn new(opts: OllamaEmbedderOpts) -> Result<Self, LunarisError> {
        let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("ollama client: {e}")))
        })?;
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

        let resp =
            self.client.post(&url).json(&body).send().await.map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!("ollama: {e}")))
            })?;
        if !resp.status().is_success() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "ollama: HTTP {}",
                resp.status()
            ))));
        }
        let parsed: EmbedResponse = resp.json().await.map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("ollama parse: {e}")))
        })?;

        // Response-shape validation (T-02-01-04 mitigation).
        if parsed.embeddings.len() != inputs.len() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "ollama returned wrong shape: expected {} rows, got {}",
                inputs.len(),
                parsed.embeddings.len()
            ))));
        }
        for (i, row) in parsed.embeddings.iter().enumerate() {
            if row.len() != self.dim {
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "ollama returned wrong shape: row {} has dim {} (expected {})",
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
        let opts = OllamaEmbedderOpts::default();
        assert_eq!(opts.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(opts.model, DEFAULT_MODEL);
        assert_eq!(opts.dim, 768);
    }

    #[test]
    fn embedder_construction_succeeds_with_defaults() {
        let _e = OllamaEmbedder::new(OllamaEmbedderOpts::default()).expect("client builds");
    }
}
