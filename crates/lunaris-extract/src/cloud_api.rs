//! [`CloudApiExtractor`] — provider-mux extractor for Anthropic / OpenAI /
//! Gemini.
//!
//! Phase 12a duplication-delete: the per-provider HTTP client, request
//! builders, and response decoders that previously lived here have been
//! consolidated into `lunaris_llm::CloudBackend`. This file retains only:
//!
//! - The public API types: [`CloudApiExtractor`], [`CloudApiExtractorOpts`],
//!   [`CloudProvider`] (re-exported from this module unchanged).
//! - The `Extractor` impl, which preserves the D-21 sentinel-on-retry-exhaust
//!   contract — see note below.
//!
//! ## Why CloudApiExtractor does NOT fully delegate to LlmExtractor
//!
//! `LlmExtractor::extract_one` swallows all backend errors into an empty
//! extraction (same strategy as the candle and Ollama backends). That is the
//! right default for local backends, but for the cloud-API path D-21 requires
//! that retry exhaustion produce a SENTINEL entity
//! (`entity_type = "__lunaris_sentinel__"`, `name = "__transient_after_retry__"`)
//! that the validator routes to `NeedsReviewReason::TransientAfterRetry`.
//!
//! To preserve this contract we call `backend.generate()` directly per chunk
//! and wrap any error (after `CloudBackend`'s own internal retry) into the
//! D-21 sentinel. The batch-level D-02 timeout wraps the whole loop exactly
//! as before.
//!
//! ## Failure modes (unchanged)
//!
//! | Condition                                    | Behaviour                                                        |
//! |----------------------------------------------|------------------------------------------------------------------|
//! | HTTP 429 / 5xx / network / timeout           | `CloudBackend` retries once (D-21), then returns `LunarisError` |
//! | `LunarisError` from `generate()`             | Sentinel entity emitted; validator routes → TransientAfterRetry  |
//! | HTTP 4xx (auth, invalid)                     | `LunarisError` bubbled; no retry                                 |
//! | Batch timeout (D-02)                         | Falls back to per-chunk extraction                               |

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::{LunarisError, StorageError};
use lunaris_llm::{CloudBackend, CloudBackendOpts, GenOpts, LlmBackend, SchemaConstraint};
use ulid::Ulid;

use crate::Extractor;
use crate::types::{ChunkInput, Entity, EntityId, RawExtraction, RawExtractionBatch};
use crate::validator::{TRANSIENT_SENTINEL_NAME, TRANSIENT_SENTINEL_TYPE};

/// Default per-batch timeout (D-02).
const DEFAULT_BATCH_TIMEOUT_MS: u64 = 150;

/// Default retry budget per D-21.
const DEFAULT_MAX_RETRIES: u8 = 1;

/// Selectable provider per D-01. Reads from `LUNARIS_EXTRACT_PROVIDER` env
/// (case-insensitive) at [`CloudApiExtractorOpts::default`] time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudProvider {
    Anthropic,
    OpenAI,
    Gemini,
}

impl FromStr for CloudProvider {
    type Err = LunarisError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "openai" | "gpt" => Ok(Self::OpenAI),
            "gemini" | "google" => Ok(Self::Gemini),
            other => Err(LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api: unknown provider {other:?} (expected anthropic|openai|gemini)"
            )))),
        }
    }
}

impl CloudProvider {
    fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-3-5-haiku-latest",
            Self::OpenAI => "gpt-4o-mini",
            Self::Gemini => "gemini-2.5-flash",
        }
    }
    fn api_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAI => "OPENAI_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
        }
    }
    fn model_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_EXTRACT_MODEL",
            Self::OpenAI => "OPENAI_EXTRACT_MODEL",
            Self::Gemini => "GEMINI_EXTRACT_MODEL",
        }
    }
}

/// Bridge from this module's `CloudProvider` to `lunaris_llm::CloudProvider`.
/// Private — callers only see the extract-side type.
impl From<CloudProvider> for lunaris_llm::CloudProvider {
    fn from(p: CloudProvider) -> Self {
        match p {
            CloudProvider::Anthropic => lunaris_llm::CloudProvider::Anthropic,
            CloudProvider::OpenAI => lunaris_llm::CloudProvider::OpenAI,
            CloudProvider::Gemini => lunaris_llm::CloudProvider::Gemini,
        }
    }
}

/// Construction options for [`CloudApiExtractor`].
///
/// `Default` reads provider from `LUNARIS_EXTRACT_PROVIDER` (defaults to
/// Anthropic), model from `<PROVIDER>_EXTRACT_MODEL` (defaults to the
/// provider's default), and api_key from `<PROVIDER>_API_KEY` (empty string
/// if unset — `new` will reject empty keys with an actionable error).
#[derive(Clone, Debug)]
pub struct CloudApiExtractorOpts {
    pub provider: CloudProvider,
    pub model: String,
    pub api_key: String,
    pub batch_timeout_ms: u64,
    pub max_retries: u8,
}

impl Default for CloudApiExtractorOpts {
    fn default() -> Self {
        let provider = std::env::var("LUNARIS_EXTRACT_PROVIDER")
            .ok()
            .and_then(|s| CloudProvider::from_str(&s).ok())
            .unwrap_or(CloudProvider::Anthropic);
        let model = std::env::var(provider.model_env())
            .unwrap_or_else(|_| provider.default_model().to_string());
        let api_key = std::env::var(provider.api_key_env()).unwrap_or_default();
        Self {
            provider,
            model,
            api_key,
            batch_timeout_ms: DEFAULT_BATCH_TIMEOUT_MS,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

/// Cloud-API extractor — wraps `lunaris_llm::CloudBackend` per chunk and
/// emits the D-21 sentinel on retry exhaust.
#[derive(Clone)]
pub struct CloudApiExtractor {
    backend: Arc<CloudBackend>,
    provider: CloudProvider,
    batch_timeout_ms: u64,
    /// Per-call GenOpts — max_tokens and temperature use the
    /// LlmExtractorOpts defaults (512 / 0.0). timeout is set to the full
    /// batch budget so CloudBackend's own D-21 retry stays within D-02.
    gen_opts: GenOpts,
}

impl std::fmt::Debug for CloudApiExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // T-03-01-03 mitigation: NEVER log the api_key. Print only the
        // provider + model so a `tracing::debug!(?extractor)` line stays safe.
        f.debug_struct("CloudApiExtractor")
            .field("provider", &self.provider)
            .field("model_id", &self.backend.model_id())
            .field("batch_timeout_ms", &self.batch_timeout_ms)
            .finish()
    }
}

impl CloudApiExtractor {
    /// Construct a new cloud-API extractor.
    pub fn new(opts: CloudApiExtractorOpts) -> Result<Self, LunarisError> {
        let llm_provider = lunaris_llm::CloudProvider::from(opts.provider);
        let backend_opts = CloudBackendOpts {
            provider: llm_provider,
            model: opts.model,
            api_key: opts.api_key,
            max_retries: opts.max_retries,
        };
        let backend = Arc::new(CloudBackend::new(backend_opts)?);
        // Per-call timeout = full batch budget so the internal retry in
        // CloudBackend::generate stays bounded within D-02.
        let gen_opts = GenOpts {
            max_tokens: 512,
            temperature: 0.0,
            timeout: Duration::from_millis(opts.batch_timeout_ms),
        };
        Ok(Self { backend, provider: opts.provider, batch_timeout_ms: opts.batch_timeout_ms, gen_opts })
    }

    /// Single chunk with D-21 sentinel-on-error. The `CloudBackend` already
    /// handles the internal retry budget (max_retries from opts). When
    /// `generate()` returns `Err`, we emit the transient sentinel so the
    /// validator can route it to `NeedsReviewReason::TransientAfterRetry`.
    async fn extract_one_with_sentinel(&self, chunk: &ChunkInput) -> RawExtraction {
        let prompt = format!(
            "Extract entities and relations from the chunk below. Respond with \
             a JSON object {{\"entities\":[...],\"relations\":[...]}} only.\n\n\
             <chunk heading=\"{}\">\n{}\n</chunk>",
            chunk.heading_path.join(" / "),
            chunk.text
        );
        match self
            .backend
            .generate(&prompt, SchemaConstraint::None, self.gen_opts)
            .await
        {
            Ok(text) => crate::llm_extractor::parse_extraction_json_pub(&text, chunk.chunk_id),
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    chunk_id = %chunk.chunk_id,
                    model_id = self.backend.model_id(),
                    "cloud-api retry exhausted; emitting transient-after-retry sentinel"
                );
                let err_text = e.to_string();
                let sentinel = Entity {
                    id: EntityId::from_name_and_type(
                        TRANSIENT_SENTINEL_NAME,
                        TRANSIENT_SENTINEL_TYPE,
                    ),
                    name: TRANSIENT_SENTINEL_NAME.into(),
                    aliases: Vec::new(),
                    entity_type: TRANSIENT_SENTINEL_TYPE.into(),
                    confidence: 0.0,
                    valid_from_iso: format!("transient: {err_text}"),
                    valid_to_iso: None,
                };
                RawExtraction {
                    source_chunk_id: chunk.chunk_id,
                    entities: vec![sentinel],
                    relations: Vec::new(),
                    facts: Vec::new(),
                }
            }
        }
    }
}

#[async_trait]
impl Extractor for CloudApiExtractor {
    async fn extract(
        &self,
        _episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        if chunks.is_empty() {
            return Ok(RawExtractionBatch::default());
        }

        // Per-batch timeout (D-02). On timeout we fall back to per-chunk
        // (each per-chunk call has its own retry budget inside CloudBackend).
        let batch_timeout = Duration::from_millis(self.batch_timeout_ms);
        let chunks_owned: Vec<ChunkInput> = chunks.to_vec();
        let this = self.clone();
        let batch_fut = async move {
            let mut by_chunk = Vec::with_capacity(chunks_owned.len());
            for c in &chunks_owned {
                by_chunk.push(this.extract_one_with_sentinel(c).await);
            }
            RawExtractionBatch { by_chunk }
        };

        match tokio::time::timeout(batch_timeout, batch_fut).await {
            Ok(b) => Ok(b),
            Err(_elapsed) => {
                tracing::warn!(
                    batch_size = chunks.len(),
                    timeout_ms = self.batch_timeout_ms,
                    "cloud-api batch timeout; falling back to per-chunk"
                );
                let mut by_chunk = Vec::with_capacity(chunks.len());
                for c in chunks {
                    by_chunk.push(self.extract_one_with_sentinel(c).await);
                }
                Ok(RawExtractionBatch { by_chunk })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_provider_from_str_parses_all_three() {
        assert_eq!(CloudProvider::from_str("anthropic").unwrap(), CloudProvider::Anthropic);
        assert_eq!(CloudProvider::from_str("Claude").unwrap(), CloudProvider::Anthropic);
        assert_eq!(CloudProvider::from_str("openai").unwrap(), CloudProvider::OpenAI);
        assert_eq!(CloudProvider::from_str("GPT").unwrap(), CloudProvider::OpenAI);
        assert_eq!(CloudProvider::from_str("gemini").unwrap(), CloudProvider::Gemini);
        assert_eq!(CloudProvider::from_str("Google").unwrap(), CloudProvider::Gemini);
    }

    #[test]
    fn cloud_provider_from_str_rejects_unknown() {
        let err = CloudProvider::from_str("deepseek").unwrap_err();
        assert!(err.to_string().contains("unknown provider"));
    }

    #[test]
    fn empty_api_key_rejected() {
        let opts = CloudApiExtractorOpts {
            provider: CloudProvider::Anthropic,
            model: "claude-3-5-haiku-latest".into(),
            api_key: "".into(),
            batch_timeout_ms: 150,
            max_retries: 1,
        };
        let err = CloudApiExtractor::new(opts).expect_err("empty key must error");
        let msg = err.to_string();
        assert!(msg.contains("api_key is empty"), "got: {msg}");
        assert!(msg.contains("ANTHROPIC_API_KEY"), "got: {msg}");
    }

    #[test]
    fn debug_impl_redacts_api_key() {
        // Construct with a fake key and prove Debug doesn't print it.
        let opts = CloudApiExtractorOpts {
            provider: CloudProvider::Anthropic,
            model: "claude-3-5-haiku-latest".into(),
            api_key: "sk-ant-SECRET-KEY-ABCDEF".into(),
            batch_timeout_ms: 150,
            max_retries: 1,
        };
        let extractor = CloudApiExtractor::new(opts).unwrap();
        let dbg = format!("{extractor:?}");
        assert!(!dbg.contains("SECRET"), "Debug must redact api_key, got: {dbg}");
        assert!(!dbg.contains("sk-ant"), "Debug must redact api_key, got: {dbg}");
    }

    #[test]
    fn cloud_provider_bridge_maps_all_three() {
        assert!(matches!(
            lunaris_llm::CloudProvider::from(CloudProvider::Anthropic),
            lunaris_llm::CloudProvider::Anthropic
        ));
        assert!(matches!(
            lunaris_llm::CloudProvider::from(CloudProvider::OpenAI),
            lunaris_llm::CloudProvider::OpenAI
        ));
        assert!(matches!(
            lunaris_llm::CloudProvider::from(CloudProvider::Gemini),
            lunaris_llm::CloudProvider::Gemini
        ));
    }
}
