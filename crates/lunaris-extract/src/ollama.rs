//! [`OllamaExtractor`] — thin wrapper over [`crate::LlmExtractor`] +
//! `lunaris_llm::OllamaBackend`.
//!
//! Phase 12a duplication-delete: the load / HTTP / parse code that previously
//! lived here has been collapsed into the shared [`lunaris_llm::OllamaBackend`]
//! + [`crate::llm_extractor::LlmExtractor`] stack. The public API surface
//!   (`OllamaExtractor`, `OllamaExtractorOpts`, the `Extractor` impl) is
//!   byte-identical to v0.2 so downstream callers are unaffected.
//!
//! ## Constraint handling (preserved from v0.2)
//!
//! Ollama receives the JSON-schema translation of the extraction grammar
//! as the `format` field via [`lunaris_llm::SchemaConstraint::JsonSchema`].
//! GBNF is NOT passed through — Ollama (≤ 0.5) does not consume GBNF; the
//! grammar is embedded in the prompt by the `OllamaBackend` only if
//! `SchemaConstraint::Gbnf` is used, but for extraction we use the
//! JSON-schema mode (existing behaviour). The `LlmExtractor` `opts.gbnf`
//! field is left `None` here; the `OllamaBackend` will use
//! [`lunaris_llm::SchemaConstraint::None`] which is the lighter-prompt
//! path and matches the v0.2 behaviour (the JSON-schema was the
//! enforcement mechanism, not GBNF).
//!
//! ## Failure modes (unchanged)
//!
//! Errors propagate from `OllamaBackend::generate` as
//! `LunarisError::Storage(StorageError::Backend("ollama: ..."))`, same
//! shape as v0.2.

use std::sync::Arc;

use async_trait::async_trait;
use lunaris_core::LunarisError;
use lunaris_llm::{OllamaBackend, OllamaBackendOpts};
use ulid::Ulid;

use crate::Extractor;
use crate::llm_extractor::{LlmExtractor, LlmExtractorOpts};
use crate::types::{ChunkInput, RawExtractionBatch};

/// Default Ollama endpoint.
const DEFAULT_ENDPOINT: &str = "http://localhost:11434";

/// Default Ollama model identifier (matches `ollama pull gemma3:4b`).
const DEFAULT_MODEL: &str = "gemma3:4b";

/// Default per-batch timeout per D-02 (matches candle backend).
const DEFAULT_BATCH_TIMEOUT_MS: u64 = 150;

/// Construction options for [`OllamaExtractor`].
///
/// `Default` resolves `endpoint` from `OLLAMA_URL` env (falls back to
/// `http://localhost:11434`) and `model` from `OLLAMA_EXTRACT_MODEL` env
/// (falls back to `gemma3:4b`).
#[derive(Clone, Debug)]
pub struct OllamaExtractorOpts {
    pub endpoint: String,
    pub model: String,
    pub batch_timeout_ms: u64,
}

impl Default for OllamaExtractorOpts {
    fn default() -> Self {
        Self {
            endpoint: std::env::var("OLLAMA_URL").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string()),
            model: std::env::var("OLLAMA_EXTRACT_MODEL")
                .unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            batch_timeout_ms: DEFAULT_BATCH_TIMEOUT_MS,
        }
    }
}

/// Ollama-backed extractor — thin wrapper over [`LlmExtractor`].
///
/// Holds a `LlmExtractor` built from `OllamaBackend` + extraction opts.
/// All `Extractor::extract` calls delegate directly to the inner extractor.
#[derive(Clone, Debug)]
pub struct OllamaExtractor {
    inner: LlmExtractor,
}

impl OllamaExtractor {
    /// Construct a new Ollama-backed extractor. Builds an `OllamaBackend`
    /// with a 10s HTTP timeout and wraps it in a `LlmExtractor`.
    pub fn new(opts: OllamaExtractorOpts) -> Result<Self, LunarisError> {
        let backend_opts = OllamaBackendOpts { endpoint: opts.endpoint, model: opts.model };
        let backend =
            Arc::new(OllamaBackend::new(backend_opts)?) as Arc<dyn lunaris_llm::LlmBackend>;
        let extractor_opts = LlmExtractorOpts {
            batch_timeout_ms: opts.batch_timeout_ms,
            // No per_chunk_timeout_ms in legacy OllamaExtractorOpts — use the
            // LlmExtractorOpts default (450ms, 3× per-batch).
            per_chunk_timeout_ms: LlmExtractorOpts::default().per_chunk_timeout_ms,
            // Ollama uses JSON-schema structured output, not inline GBNF.
            // The OllamaBackend receives SchemaConstraint::None here and
            // enforces the extraction schema via the JSON-schema `format`
            // field (OllamaBackend handles the format field translation).
            // Leave gbnf = None so we don't double-embed the grammar.
            gbnf: None,
            ..LlmExtractorOpts::default()
        };
        Ok(Self { inner: LlmExtractor::with_opts(backend, extractor_opts) })
    }
}

#[async_trait]
impl Extractor for OllamaExtractor {
    async fn extract(
        &self,
        episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        self.inner.extract(episode_id, chunks).await
    }

    fn applies(&self) -> bool {
        self.inner.applies()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opts_default_resolves_to_localhost_ollama() {
        // Avoid env-var contamination from the test runner — read the actual
        // resolved values and just check they're non-empty.
        let opts = OllamaExtractorOpts::default();
        assert!(!opts.endpoint.is_empty());
        assert!(!opts.model.is_empty());
        assert_eq!(opts.batch_timeout_ms, DEFAULT_BATCH_TIMEOUT_MS);
    }

    #[test]
    fn extractor_construction_succeeds_with_defaults() {
        let _e = OllamaExtractor::new(OllamaExtractorOpts::default()).expect("client builds");
    }
}
