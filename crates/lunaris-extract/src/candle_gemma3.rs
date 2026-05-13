//! [`CandleGemma3_4B`] — Gemma-3 4B instruction-tuned extractor backed by
//! candle 0.10's typed `candle_transformers::models::gemma3::Model`.
//!
//! Phase 12a duplication-delete: the model load, greedy forward pass, and
//! post-hoc JSON parse code that previously lived here has been consolidated
//! into `lunaris_llm::CandleBackend` + `crate::llm_extractor::LlmExtractor`.
//! This file retains only the public API surface so downstream callers are
//! unaffected:
//!
//! - [`CandleGemma3_4B`] (struct + constructors)
//! - [`CandleGemma3_4BOpts`] (options struct with all original fields)
//! - [`ENTITIES_GBNF`] / [`RELATIONS_GBNF`] (pub consts — parity gate imports)
//! - Default per-batch and per-chunk timeout constants
//!
//! ## Cache-miss contract (load-bearing, CLAUDE.md §D-01)
//!
//! The error string `"gemma-3-4b-it weights missing at PATH — run
//! `huggingface-cli download google/gemma-3-4b-it --local-dir PATH`"` is
//! preserved bit-for-bit because `CandleBackend::new` with
//! `model_name = "gemma-3-4b-it"` produces exactly that format. The umbrella
//! `Lunaris::open` matches on this string to substitute [`crate::NoopExtractor`].
//!
//! ## GBNF contract (D-04 / D-05)
//!
//! `LlmExtractorOpts.gbnf = Some(FULL_GBNF)` so the wrapper inlines the
//! combined entity + relation grammars into the prompt, matching the legacy
//! `extract_one` prompt exactly. `FULL_GBNF` is built at compile time via
//! `concat!(include_str!(...), "\n", include_str!(...))`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::Device;
use lunaris_core::LunarisError;
use lunaris_llm::{CandleBackend, CandleBackendOpts};
use ulid::Ulid;

use crate::Extractor;
use crate::llm_extractor::{LlmExtractor, LlmExtractorOpts};
use crate::types::{ChunkInput, RawExtractionBatch};

/// Default sub-directory under the user's cache root.
const DEFAULT_CACHE_SUBDIR: &str = "lunaris/models/gemma-3-4b-it";

/// Default per-batch timeout per D-02.
pub const DEFAULT_BATCH_TIMEOUT_MS: u64 = 150;

/// Default per-chunk timeout for the per-chunk fallback path.
pub const DEFAULT_PER_CHUNK_TIMEOUT_MS: u64 = 450;

/// Default max-new-tokens cap for the decode loop.
pub const DEFAULT_MAX_NEW_TOKENS: usize = 512;

/// GBNF grammar source-of-truth for entity extraction (D-04).
pub const ENTITIES_GBNF: &str = include_str!("../grammars/entities.gbnf");

/// GBNF grammar source-of-truth for relation extraction (D-04).
pub const RELATIONS_GBNF: &str = include_str!("../grammars/relations.gbnf");

/// Combined GBNF grammar — entities + relations concatenated. Passed to
/// [`LlmExtractorOpts::gbnf`] so the wrapper inlines the same grammar text
/// into the prompt that the legacy `extract_one` did (D-04 / D-05 parity).
///
/// Built at compile time via `concat!` + `include_str!`; `&'static str`.
const FULL_GBNF: &str = concat!(
    include_str!("../grammars/entities.gbnf"),
    "\n",
    include_str!("../grammars/relations.gbnf"),
);

/// Construction options for [`CandleGemma3_4B`].
///
/// `Default` resolves `model_path` to `~/.cache/lunaris/models/gemma-3-4b-it/`,
/// `device` to `Device::Cpu`, `batch_timeout_ms` to
/// [`DEFAULT_BATCH_TIMEOUT_MS`] (D-02), `per_chunk_timeout_ms` to
/// [`DEFAULT_PER_CHUNK_TIMEOUT_MS`], and `max_new_tokens` to
/// [`DEFAULT_MAX_NEW_TOKENS`].
#[derive(Clone, Debug)]
pub struct CandleGemma3_4BOpts {
    /// Filesystem path containing `tokenizer.json`, `config.json`, and
    /// `model.safetensors`. `None` resolves to the default cache subdir.
    pub model_path: Option<PathBuf>,
    /// candle compute device. v0 ships `Device::Cpu`.
    pub device: Device,
    /// Per-batch timeout (D-02). On timeout the extractor falls back to
    /// per-chunk extraction.
    pub batch_timeout_ms: u64,
    /// Per-chunk timeout for the per-chunk fallback path.
    pub per_chunk_timeout_ms: u64,
    /// Max tokens emitted per chunk's decode loop.
    pub max_new_tokens: usize,
}

impl Default for CandleGemma3_4BOpts {
    fn default() -> Self {
        let cache_root = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            model_path: Some(cache_root.join(DEFAULT_CACHE_SUBDIR)),
            device: Device::Cpu,
            batch_timeout_ms: DEFAULT_BATCH_TIMEOUT_MS,
            per_chunk_timeout_ms: DEFAULT_PER_CHUNK_TIMEOUT_MS,
            max_new_tokens: DEFAULT_MAX_NEW_TOKENS,
        }
    }
}

/// Real Gemma-3 4B extractor — thin wrapper over
/// [`LlmExtractor`]`(`[`CandleBackend`]`)`.
///
/// Construction is async because tokenizer + safetensors load synchronously
/// hit the filesystem; we wrap the load in `tokio::task::spawn_blocking`
/// (inside `CandleBackend::new`) to avoid stalling the runtime on a cold cache.
///
/// Returns the actionable cache-miss error
/// ```text
/// gemma-3-4b-it weights missing at PATH — run `huggingface-cli download google/gemma-3-4b-it --local-dir PATH`
/// ```
/// when `tokenizer.json`, `config.json`, or `model.safetensors` are missing.
#[derive(Clone, Debug)]
pub struct CandleGemma3_4B {
    inner: LlmExtractor,
}

impl CandleGemma3_4B {
    /// Construct from the default cache.
    pub async fn try_new_from_default_cache() -> Result<Self, LunarisError> {
        Self::new(CandleGemma3_4BOpts::default()).await
    }

    /// Construct from an explicit model path (escape hatch for tests + non-
    /// default cache layouts). Other options take their defaults.
    pub async fn try_new_from_path(model_path: PathBuf) -> Result<Self, LunarisError> {
        Self::new(CandleGemma3_4BOpts {
            model_path: Some(model_path),
            ..CandleGemma3_4BOpts::default()
        })
        .await
    }

    /// Construct with full options.
    pub async fn new(opts: CandleGemma3_4BOpts) -> Result<Self, LunarisError> {
        let model_path = opts
            .model_path
            .clone()
            .unwrap_or_else(|| CandleGemma3_4BOpts::default().model_path.unwrap());

        let backend_opts = CandleBackendOpts {
            model_name: "gemma-3-4b-it".into(),
            model_path,
            device: opts.device,
        };
        let backend = Arc::new(CandleBackend::new(backend_opts).await?);

        let extractor_opts = LlmExtractorOpts {
            batch_timeout_ms: opts.batch_timeout_ms,
            per_chunk_timeout_ms: opts.per_chunk_timeout_ms,
            max_tokens: opts.max_new_tokens as u32,
            temperature: 0.0,
            // D-04 / D-05 parity: inline the combined entity + relation GBNF
            // grammar into every prompt, exactly as the legacy extract_one did.
            gbnf: Some(FULL_GBNF),
        };
        Ok(Self { inner: LlmExtractor::with_opts(backend, extractor_opts) })
    }
}

#[async_trait]
impl Extractor for CandleGemma3_4B {
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
    fn opts_default_resolves_to_cache_subdir() {
        let opts = CandleGemma3_4BOpts::default();
        let path = opts.model_path.expect("default sets a path");
        let s = path.to_string_lossy().to_string();
        assert!(
            s.contains("lunaris") && s.contains("models") && s.contains("gemma-3-4b-it"),
            "default model_path should include the v0 cache layout, got: {s}"
        );
        assert_eq!(opts.batch_timeout_ms, DEFAULT_BATCH_TIMEOUT_MS);
        assert_eq!(opts.per_chunk_timeout_ms, DEFAULT_PER_CHUNK_TIMEOUT_MS);
        assert_eq!(opts.max_new_tokens, DEFAULT_MAX_NEW_TOKENS);
    }

    #[test]
    fn grammars_loaded_via_include_str() {
        // Sanity — both grammars loaded at compile time and contain the
        // expected `root` rule.
        assert!(ENTITIES_GBNF.contains("root"));
        assert!(ENTITIES_GBNF.contains("name"));
        assert!(RELATIONS_GBNF.contains("root"));
        assert!(RELATIONS_GBNF.contains("predicate"));
    }

    #[test]
    fn full_gbnf_combines_both_grammars() {
        // FULL_GBNF must contain content from both grammars so the wrapper
        // passes the same combined grammar text to the backend that the
        // legacy extract_one did.
        assert!(FULL_GBNF.contains("root"), "FULL_GBNF missing root rule");
        assert!(FULL_GBNF.contains("name"), "FULL_GBNF missing entity name rule");
        assert!(FULL_GBNF.contains("predicate"), "FULL_GBNF missing relation predicate rule");
        // Both grammars' content must appear in the combined form.
        assert_eq!(
            FULL_GBNF,
            concat!(
                include_str!("../grammars/entities.gbnf"),
                "\n",
                include_str!("../grammars/relations.gbnf"),
            ),
            "FULL_GBNF must equal the expected compile-time concat"
        );
    }
}
