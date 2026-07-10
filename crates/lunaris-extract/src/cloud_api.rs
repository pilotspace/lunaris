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
    MiniMax,
    /// Any OpenAI-compatible `/chat/completions` server at a caller-supplied
    /// base URL (Ollama `/v1`, llama-server, vLLM, LM Studio) — the
    /// llama.cpp-only cutover's air-gap/local story. Base URL from
    /// `LUNARIS_OPENAI_COMPAT_BASE_URL`; API key optional.
    OpenAiCompat,
}

impl FromStr for CloudProvider {
    type Err = LunarisError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "openai" | "gpt" => Ok(Self::OpenAI),
            "gemini" | "google" => Ok(Self::Gemini),
            "minimax" => Ok(Self::MiniMax),
            "openai-compat" | "openai-compatible" => Ok(Self::OpenAiCompat),
            other => Err(LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api: unknown provider {other:?} \
                 (expected anthropic|openai|gemini|minimax|openai-compat)"
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
            Self::MiniMax => "MiniMax-M3",
            // No universal default exists for arbitrary OpenAI-compatible
            // servers — the operator names the model via
            // OPENAI_COMPAT_EXTRACT_MODEL (empty = actionable error at new()).
            Self::OpenAiCompat => "",
        }
    }
    fn api_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAI => "OPENAI_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
            Self::MiniMax => "MINIMAX_API_KEY",
            Self::OpenAiCompat => "LUNARIS_OPENAI_COMPAT_API_KEY",
        }
    }
    fn model_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_EXTRACT_MODEL",
            Self::OpenAI => "OPENAI_EXTRACT_MODEL",
            Self::Gemini => "GEMINI_EXTRACT_MODEL",
            Self::MiniMax => "MINIMAX_EXTRACT_MODEL",
            Self::OpenAiCompat => "OPENAI_COMPAT_EXTRACT_MODEL",
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
            CloudProvider::MiniMax => lunaris_llm::CloudProvider::MiniMax,
            CloudProvider::OpenAiCompat => lunaris_llm::CloudProvider::OpenAiCompat,
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
    /// Per-call max output tokens. Defaults to 512 (LlmExtractorOpts's
    /// historical implicit value) -- unchanged for existing callers. A
    /// reasoning-heavy cloud model can exhaust 512 tokens on its own
    /// reasoning before emitting the JSON answer (confirmed against
    /// MiniMax-M3 via the LongMemEval graph-pipeline prototype, 2026-07:
    /// `finish_reason: length`, empty content) -- raise this explicitly
    /// for such models.
    pub max_tokens: u32,
    /// Max per-chunk extraction calls in flight (order-preserving; see
    /// [`DEFAULT_EXTRACT_CONCURRENCY`] for the measured rationale). 1 =
    /// the historical strictly-serial loop.
    pub concurrency: usize,
    /// Base URL for [`CloudProvider::OpenAiCompat`]; ignored by the
    /// fixed-endpoint providers. `Default` reads
    /// `LUNARIS_OPENAI_COMPAT_BASE_URL`.
    pub base_url: Option<String>,
}

/// Historical implicit default (was hardcoded in [`CloudApiExtractor::new`]).
const DEFAULT_MAX_TOKENS: u32 = 512;

/// Default bounded concurrency for per-chunk cloud extraction calls.
///
/// The 2026-07-10 LongMemEval flame investigation root-caused ~95% of
/// per-question wall time to this file's previously strictly-serial
/// per-chunk loop (~11s per MiniMax-M3 completion, ~40 chunks, one at a
/// time, process idle in `__psynch_cvwait`). A live probe measured 3.8x
/// overlap at 4 concurrent calls with zero rate-limit errors. Cloud calls
/// are independent HTTP requests, so bounded overlap is safe; 4 keeps a
/// comfortable margin under provider rate limits. Set to 1 to restore the
/// historical serial behavior.
const DEFAULT_EXTRACT_CONCURRENCY: usize = 4;

/// Drive the given per-chunk extraction futures with at most `concurrency`
/// in flight, preserving input order in the returned vec (`out[i]`
/// corresponds to `futs[i]` — downstream consumers align by index).
/// Futures are lazy: building the full Vec up front costs nothing until
/// `buffered` polls them. `concurrency` is clamped to at least 1.
async fn extract_chunks_buffered<Fut>(futs: Vec<Fut>, concurrency: usize) -> Vec<RawExtraction>
where
    Fut: std::future::Future<Output = RawExtraction>,
{
    use futures::StreamExt;
    futures::stream::iter(futs).buffered(concurrency.max(1)).collect().await
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
        let base_url = match provider {
            CloudProvider::OpenAiCompat => {
                std::env::var("LUNARIS_OPENAI_COMPAT_BASE_URL").ok().filter(|s| !s.is_empty())
            }
            _ => None,
        };
        Self {
            provider,
            model,
            api_key,
            batch_timeout_ms: DEFAULT_BATCH_TIMEOUT_MS,
            max_retries: DEFAULT_MAX_RETRIES,
            max_tokens: DEFAULT_MAX_TOKENS,
            concurrency: DEFAULT_EXTRACT_CONCURRENCY,
            base_url,
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
    /// Per-call GenOpts — max_tokens from `opts.max_tokens` (default 512),
    /// temperature fixed at 0.0. timeout is set to the full batch budget so
    /// CloudBackend's own D-21 retry stays within D-02.
    gen_opts: GenOpts,
    /// Bounded per-chunk extraction concurrency (see CloudApiExtractorOpts).
    concurrency: usize,
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
        // openai-compat has no universal default model — fail fast with the
        // env name instead of sending an empty model string to the server.
        // (Key/base-url validation lives in lunaris_llm::CloudBackend::new:
        // empty key is ALLOWED for openai-compat, base_url is required.)
        if opts.provider == CloudProvider::OpenAiCompat && opts.model.trim().is_empty() {
            return Err(LunarisError::Storage(StorageError::Backend(
                "cloud-api: openai-compat model is empty — set OPENAI_COMPAT_EXTRACT_MODEL \
                 (or CloudApiExtractorOpts.model) to the model your server hosts"
                    .to_string(),
            )));
        }
        let llm_provider = lunaris_llm::CloudProvider::from(opts.provider);
        let backend_opts = CloudBackendOpts {
            provider: llm_provider,
            model: opts.model,
            api_key: opts.api_key,
            max_retries: opts.max_retries,
            base_url: opts.base_url,
        };
        let backend = Arc::new(CloudBackend::new(backend_opts)?);
        // Per-call timeout = full batch budget so the internal retry in
        // CloudBackend::generate stays bounded within D-02.
        let gen_opts = GenOpts {
            max_tokens: opts.max_tokens,
            temperature: 0.0,
            timeout: Duration::from_millis(opts.batch_timeout_ms),
        };
        Ok(Self {
            backend,
            provider: opts.provider,
            batch_timeout_ms: opts.batch_timeout_ms,
            gen_opts,
            concurrency: opts.concurrency.max(1),
        })
    }

    /// Single chunk with D-21 sentinel-on-error. The `CloudBackend` already
    /// handles the internal retry budget (max_retries from opts). When
    /// `generate()` returns `Err`, we emit the transient sentinel so the
    /// validator can route it to `NeedsReviewReason::TransientAfterRetry`.
    async fn extract_one_with_sentinel(&self, chunk: &ChunkInput) -> RawExtraction {
        // Shared with llm_extractor.rs -- this file used to carry its own
        // independent, equally vague prompt (no field names), reproducing
        // the exact "missing field entity_type" parse-failure bug found
        // and fixed there (LongMemEval graph-pipeline prototype, 2026-07).
        let prompt = crate::llm_extractor::build_prompt(chunk);
        match self.backend.generate(&prompt, SchemaConstraint::None, self.gen_opts).await {
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
        let concurrency = self.concurrency;
        let batch_fut = async move {
            let futs: Vec<_> =
                chunks_owned.iter().map(|c| this.extract_one_with_sentinel(c)).collect();
            let by_chunk = extract_chunks_buffered(futs, concurrency).await;
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
                let futs: Vec<_> =
                    chunks.iter().map(|c| self.extract_one_with_sentinel(c)).collect();
                let by_chunk = extract_chunks_buffered(futs, self.concurrency).await;
                Ok(RawExtractionBatch { by_chunk })
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

    #[tokio::test]
    async fn extract_chunks_buffered_overlaps_up_to_concurrency_and_preserves_order() {
        // The 2026-07-10 benchmark flame investigation root-caused the LME
        // graph pipeline's 8-min questions to THIS file's strictly serial
        // per-chunk `.await` loop: ~11s per MiniMax completion × ~40 chunks,
        // one at a time, while the process sat in __psynch_cvwait. The live
        // probe measured 3.8x overlap at 4 concurrent calls with zero
        // rate-limit errors — bounded buffering is a pure win for cloud
        // backends. Output order MUST still match input order (by_chunk[i]
        // corresponds to chunks[i] downstream).
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let chunks: Vec<ChunkInput> = (0..8)
            .map(|i| ChunkInput {
                chunk_id: Ulid::new(),
                text: format!("chunk {i}"),
                heading_path: vec![],
            })
            .collect();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let futs: Vec<_> = chunks
            .iter()
            .map(|c| {
                let id = c.chunk_id;
                let in_flight = Arc::clone(&in_flight);
                let max_seen = Arc::clone(&max_seen);
                async move {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    RawExtraction { source_chunk_id: id, ..Default::default() }
                }
            })
            .collect();
        let out = extract_chunks_buffered(futs, 4).await;
        assert_eq!(out.len(), chunks.len());
        for (c, r) in chunks.iter().zip(&out) {
            assert_eq!(c.chunk_id, r.source_chunk_id, "buffered output must preserve input order");
        }
        let peak = max_seen.load(Ordering::SeqCst);
        assert!(peak >= 3, "expected >=3 overlapping extractions at concurrency 4, saw {peak}");
    }

    #[tokio::test]
    async fn extract_chunks_buffered_concurrency_1_stays_strictly_serial() {
        // concurrency=1 must reproduce the historical serial behavior exactly
        // (local-backend callers rely on it — a model-mutex-bound backend
        // gains nothing from overlap and would only accrue per-chunk-timeout
        // exposure while queued).
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let chunks: Vec<ChunkInput> = (0..4)
            .map(|i| ChunkInput {
                chunk_id: Ulid::new(),
                text: format!("chunk {i}"),
                heading_path: vec![],
            })
            .collect();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let futs: Vec<_> = chunks
            .iter()
            .map(|c| {
                let id = c.chunk_id;
                let in_flight = Arc::clone(&in_flight);
                let max_seen = Arc::clone(&max_seen);
                async move {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    RawExtraction { source_chunk_id: id, ..Default::default() }
                }
            })
            .collect();
        let out = extract_chunks_buffered(futs, 1).await;
        assert_eq!(out.len(), chunks.len());
        assert_eq!(max_seen.load(Ordering::SeqCst), 1, "concurrency=1 must never overlap");
    }

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
            max_tokens: 512,
            concurrency: 1,
            base_url: None,
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
            max_tokens: 512,
            concurrency: 1,
            base_url: None,
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

    #[test]
    fn cloud_provider_from_str_parses_minimax() {
        assert_eq!(CloudProvider::from_str("minimax").unwrap(), CloudProvider::MiniMax);
        assert_eq!(CloudProvider::from_str("MiniMax").unwrap(), CloudProvider::MiniMax);
    }

    #[test]
    fn minimax_default_model_and_envs() {
        assert_eq!(CloudProvider::MiniMax.default_model(), "MiniMax-M3");
        assert_eq!(CloudProvider::MiniMax.api_key_env(), "MINIMAX_API_KEY");
        assert_eq!(CloudProvider::MiniMax.model_env(), "MINIMAX_EXTRACT_MODEL");
    }

    #[test]
    fn cloud_provider_bridge_maps_minimax() {
        assert!(matches!(
            lunaris_llm::CloudProvider::from(CloudProvider::MiniMax),
            lunaris_llm::CloudProvider::MiniMax
        ));
    }

    #[test]
    fn openai_compat_parses_and_constructs_keyless() {
        // Cutover: the generic OpenAI-compatible URL backend must parse from
        // the provider env string AND construct without an API key.
        assert_eq!(CloudProvider::from_str("openai-compat").unwrap(), CloudProvider::OpenAiCompat);
        assert_eq!(
            CloudProvider::from_str("openai-compatible").unwrap(),
            CloudProvider::OpenAiCompat
        );
        assert!(matches!(
            lunaris_llm::CloudProvider::from(CloudProvider::OpenAiCompat),
            lunaris_llm::CloudProvider::OpenAiCompat
        ));
        let e = CloudApiExtractor::new(CloudApiExtractorOpts {
            provider: CloudProvider::OpenAiCompat,
            model: "qwen3:4b".into(),
            api_key: String::new(),
            base_url: Some("http://localhost:11434/v1".into()),
            ..CloudApiExtractorOpts::default()
        })
        .expect("keyless openai-compat extractor must construct");
        let dbg = format!("{e:?}");
        assert!(dbg.contains("OpenAiCompat"), "got: {dbg}");
    }

    #[test]
    fn openai_compat_requires_model_and_base_url() {
        let err = CloudApiExtractor::new(CloudApiExtractorOpts {
            provider: CloudProvider::OpenAiCompat,
            model: String::new(),
            api_key: String::new(),
            base_url: Some("http://localhost:11434/v1".into()),
            ..CloudApiExtractorOpts::default()
        })
        .expect_err("empty model must fail fast");
        assert!(err.to_string().contains("OPENAI_COMPAT_EXTRACT_MODEL"), "got: {err}");

        let err = CloudApiExtractor::new(CloudApiExtractorOpts {
            provider: CloudProvider::OpenAiCompat,
            model: "qwen3:4b".into(),
            api_key: String::new(),
            base_url: None,
            ..CloudApiExtractorOpts::default()
        })
        .expect_err("missing base_url must fail fast");
        assert!(err.to_string().contains("LUNARIS_OPENAI_COMPAT_BASE_URL"), "got: {err}");
    }

    #[test]
    fn max_tokens_defaults_to_512_and_is_configurable() {
        // 512 is LlmExtractorOpts's historical implicit default -- unchanged
        // for existing callers. LongMemEval graph-pipeline prototype
        // (2026-07) found MiniMax-M3 occasionally exhausts 512 tokens on
        // its own reasoning before emitting the JSON answer
        // (finish_reason: length, empty content) -- callers with a
        // reasoning-heavy cloud model need to raise this explicitly.
        let default_opts = CloudApiExtractorOpts::default();
        assert_eq!(default_opts.max_tokens, 512);
        let opts = CloudApiExtractorOpts {
            provider: CloudProvider::MiniMax,
            model: "MiniMax-M3".into(),
            api_key: "dummy".into(),
            batch_timeout_ms: 150,
            max_retries: 1,
            max_tokens: 2048,
            concurrency: 4,
            base_url: None,
        };
        let _extractor =
            CloudApiExtractor::new(opts).expect("client builds with custom max_tokens");
    }
}
