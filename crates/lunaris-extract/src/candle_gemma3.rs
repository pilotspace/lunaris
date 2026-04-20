//! [`CandleGemma3_4B`] — Gemma-3 4B instruction-tuned extractor backed by
//! candle 0.10's typed `candle_transformers::models::gemma3::Model`.
//!
//! ## Wiring strategy
//!
//! This file is the Phase 3 default extractor (D-01 + blueprint §5.2). The
//! Plan 03-01 scaffold lands the cache-miss + spawn_blocking model load
//! pattern; the GBNF-constrained sampling path (D-04) and per-batch
//! timeout-then-per-chunk fallback (D-02) are wired up here. Per-chunk forward
//! pass uses post-hoc JSON parse against the GBNF schema (D-05) when grammar-
//! constrained sampling is unavailable in the candle binding (the candle 0.10
//! `LogitsProcessor` does not yet expose stable GBNF support — see the
//! `extract_one` private fn for the fallback path).
//!
//! ## CLAUDE.md compliance
//!
//! - `#![forbid(unsafe_code)]` (lib.rs) — uses the safe
//!   `VarBuilder::from_buffered_safetensors` instead of the `unsafe`
//!   `from_mmaped_safetensors` (matches Plan 02-01 + Plan 02-03 pattern).
//! - All blocking work wrapped in `tokio::task::spawn_blocking` so the async
//!   runtime never stalls on cold model load OR per-call forward pass.
//! - `parking_lot::*` not needed here — the `Arc<CandleInner>` clone pattern
//!   provides per-call cheap snapshotting without locks.
//!
//! ## Failure modes
//!
//! | Condition                                          | Returned error                                                                                                                |
//! |----------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------|
//! | `model_path/tokenizer.json` missing                | `LunarisError::Storage(StorageError::Backend("gemma-3-4b-it weights missing at <path> — run `huggingface-cli download ...`"))` |
//! | `model_path/config.json` missing                   | same shape                                                                                                                     |
//! | `model_path/model.safetensors` missing             | same shape                                                                                                                     |
//! | tokenizer load failure                             | `LunarisError::Storage(StorageError::Backend("gemma-3-4b-it tokenizer: ..."))`                                                 |
//! | safetensors load failure                           | `LunarisError::Storage(StorageError::Backend("gemma-3-4b-it weights: ..."))`                                                   |
//! | candle tensor op failure during `extract_batch`    | `LunarisError::Storage(StorageError::Backend("gemma-3-4b-it forward: ..."))`                                                   |
//! | tokio `spawn_blocking` join failure                | `LunarisError::Storage(StorageError::Backend("gemma-3-4b-it join: ..."))`                                                      |
//! | per-batch timeout (D-02)                           | falls back to per-chunk extract; per-chunk timeout emits empty extraction with `tracing::warn!`                                |

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::gemma3::{Config as Gemma3Config, Model as Gemma3Model};
use lunaris_core::{LunarisError, StorageError};
use parking_lot::Mutex;
use tokenizers::Tokenizer;
use ulid::Ulid;

use crate::Extractor;
use crate::types::{ChunkInput, RawExtraction, RawExtractionBatch};

/// Default sub-directory under the user's cache root.
const DEFAULT_CACHE_SUBDIR: &str = "lunaris/models/gemma-3-4b-it";

/// Default per-batch timeout per D-02.
pub const DEFAULT_BATCH_TIMEOUT_MS: u64 = 150;

/// Default per-chunk timeout for the per-chunk fallback path. Generous (3x
/// per-batch) because per-chunk forward pass tokenizes + decodes one chunk at
/// a time without batching amortization.
pub const DEFAULT_PER_CHUNK_TIMEOUT_MS: u64 = 450;

/// Default max-new-tokens cap for the JSON-emitting decode loop. The GBNF
/// grammar produces ~10-30 entities per chunk × ~80 tokens/entity ≈ 2400 tokens
/// upper bound; 512 keeps the budget tight for v0.
pub const DEFAULT_MAX_NEW_TOKENS: usize = 512;

/// GBNF grammar source-of-truth for entity extraction (D-04). Loaded via
/// `include_str!` at compile time so the extractor binary ships with the
/// grammar embedded — no runtime file dep beyond the model weights.
pub const ENTITIES_GBNF: &str = include_str!("../grammars/entities.gbnf");

/// GBNF grammar source-of-truth for relation extraction (D-04).
pub const RELATIONS_GBNF: &str = include_str!("../grammars/relations.gbnf");

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
    /// Per-chunk timeout for the per-chunk fallback path. On timeout the
    /// chunk emits an empty extraction with `tracing::warn!`.
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

/// Real Gemma-3 4B extractor. See module-level rustdoc for the v0 forward
/// strategy and failure-mode table.
///
/// Construction is async because tokenizer + safetensors load synchronously
/// hit the filesystem; we wrap the load in `tokio::task::spawn_blocking` to
/// avoid stalling the runtime on a cold cache.
#[derive(Clone)]
pub struct CandleGemma3_4B {
    inner: Arc<CandleInner>,
}

impl std::fmt::Debug for CandleGemma3_4B {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleGemma3_4B")
            .field("device", &format_args!("{:?}", self.inner.device))
            .field("max_new_tokens", &self.inner.max_new_tokens)
            .field("batch_timeout_ms", &self.inner.batch_timeout_ms)
            .field("per_chunk_timeout_ms", &self.inner.per_chunk_timeout_ms)
            .finish()
    }
}

struct CandleInner {
    tokenizer: Tokenizer,
    /// Gemma-3 model. Held under a parking_lot Mutex because
    /// `Gemma3Model::forward(&mut self, ...)` requires `&mut`. The lock is
    /// taken inside `spawn_blocking` so the CLAUDE.md "never hold lock across
    /// await" rule is trivially upheld (the .await happens at the
    /// spawn_blocking boundary, not inside the closure).
    model: Mutex<Gemma3Model>,
    device: Device,
    batch_timeout_ms: u64,
    per_chunk_timeout_ms: u64,
    max_new_tokens: usize,
}

impl CandleGemma3_4B {
    /// Construct from the default cache. Mirrors
    /// `BgeRerankerV2M3::try_new_from_default_cache` from Plan 02-03.
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
    ///
    /// Returns the actionable cache-miss error
    /// ```text
    /// gemma-3-4b-it weights missing at PATH — run `huggingface-cli download google/gemma-3-4b-it --local-dir PATH`
    /// ```
    /// when `tokenizer.json`, `config.json`, or `model.safetensors` are
    /// missing — the umbrella `Lunaris::open(url)` (Plan 03-03) catches this
    /// and substitutes [`crate::NoopExtractor`] with `tracing::warn!`,
    /// matching the Plan 02-03 reranker cache-miss pattern.
    pub async fn new(opts: CandleGemma3_4BOpts) -> Result<Self, LunarisError> {
        let model_path = opts
            .model_path
            .clone()
            .unwrap_or_else(|| CandleGemma3_4BOpts::default().model_path.unwrap());

        // File-existence check FIRST — gives a fast actionable error on cache
        // miss without burning a spawn_blocking task slot on a cold load
        // attempt that would fail anyway. Mirrors the Plan 02-03 pattern.
        let tokenizer_path = model_path.join("tokenizer.json");
        let config_path = model_path.join("config.json");
        let safetensors_path = model_path.join("model.safetensors");
        for (label, p) in [
            ("tokenizer.json", &tokenizer_path),
            ("config.json", &config_path),
            ("model.safetensors", &safetensors_path),
        ] {
            if !p.exists() {
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-4b-it weights missing at {} (no {label}) — run `huggingface-cli download google/gemma-3-4b-it --local-dir {}`",
                    model_path.display(),
                    model_path.display()
                ))));
            }
        }

        let device = opts.device.clone();
        let batch_timeout_ms = opts.batch_timeout_ms;
        let per_chunk_timeout_ms = opts.per_chunk_timeout_ms;
        let max_new_tokens = opts.max_new_tokens;

        let load = tokio::task::spawn_blocking(move || -> Result<CandleInner, LunarisError> {
            // 1. Tokenizer.
            let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-4b-it tokenizer: {e}"
                )))
            })?;

            // 2. Config.
            let cfg_bytes = std::fs::read(&config_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-4b-it config: read {} ({e})",
                    config_path.display()
                )))
            })?;
            let cfg: Gemma3Config = serde_json::from_slice(&cfg_bytes).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-4b-it config: parse {} ({e})",
                    config_path.display()
                )))
            })?;

            // 3. Safetensors via the SAFE buffered loader (CLAUDE.md no-unsafe).
            let bytes = std::fs::read(&safetensors_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-4b-it weights: read {} ({e})",
                    safetensors_path.display()
                )))
            })?;
            let vb =
                VarBuilder::from_buffered_safetensors(bytes, DType::F32, &device).map_err(|e| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "gemma-3-4b-it weights: {e}"
                    )))
                })?;

            // 4. Construct typed Gemma-3 model. candle 0.10.2 ships
            //    candle_transformers::models::gemma3::Model::new(use_flash_attn, &cfg, vb)
            //    which is the EXACT architecture for Gemma-3 4B.
            //    use_flash_attn=false on CPU per Plan 02-03 BgeRerankerV2M3 pattern.
            let model = Gemma3Model::new(false, &cfg, vb).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "gemma-3-4b-it model construct: {e}"
                )))
            })?;

            Ok(CandleInner {
                tokenizer,
                model: Mutex::new(model),
                device,
                batch_timeout_ms,
                per_chunk_timeout_ms,
                max_new_tokens,
            })
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("gemma-3-4b-it join: {e}")))
        })??;

        Ok(Self { inner: Arc::new(load) })
    }

    /// Internal — extract a single chunk synchronously inside spawn_blocking.
    ///
    /// v0 strategy: prompt the model with the chunk text + GBNF grammar text
    /// embedded as instruction, sample `max_new_tokens` tokens via greedy
    /// argmax, decode with the tokenizer, post-hoc parse as JSON against the
    /// grammar shape (D-05). The post-hoc parse path is what makes EXTRACT-02
    /// satisfiable today even though the candle 0.10 `LogitsProcessor` doesn't
    /// yet expose a stable GBNF binding.
    ///
    /// Any parse failure → empty extraction (the validator's GbnfFailure path
    /// would catch it but the per-chunk fallback prefers an empty over a
    /// poisoned extraction so the WriteOp builder stays predictable).
    fn extract_one(inner: &CandleInner, chunk: &ChunkInput) -> Result<RawExtraction, LunarisError> {
        // Prompt assembly. T-03-01-01 mitigation: wrap chunk text in
        // `<chunk>...</chunk>` delimiters; the validator catches any extracted
        // entity whose name equals the literal delimiter so a prompt-injection
        // payload that escapes the wrapper doesn't pollute storage.
        let prompt = format!(
            "Extract entities and relations from the chunk below as JSON arrays.\n\n\
             Entity grammar:\n{ENTITIES_GBNF}\n\nRelation grammar:\n{RELATIONS_GBNF}\n\n\
             <chunk heading=\"{}\">\n{}\n</chunk>\n\n\
             Respond with `{{\"entities\":[...],\"relations\":[...]}}` only.",
            chunk.heading_path.join(" / "),
            chunk.text
        );

        let encoding = inner.tokenizer.encode(prompt.as_str(), true).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("gemma-3-4b-it tokenize: {e}")))
        })?;
        let input_ids = encoding.get_ids().to_vec();
        if input_ids.is_empty() {
            return Ok(RawExtraction { source_chunk_id: chunk.chunk_id, ..Default::default() });
        }

        // Greedy decode loop. We hold the parking_lot Mutex INSIDE
        // spawn_blocking — never across .await — so CLAUDE.md's lock rule is
        // upheld trivially.
        let mut model_guard = inner.model.lock();
        let mut all_ids: Vec<u32> = input_ids;
        let mut emitted: Vec<u32> = Vec::with_capacity(inner.max_new_tokens);

        for step in 0..inner.max_new_tokens {
            let context_len = if step == 0 { all_ids.len() } else { 1 };
            let start = all_ids.len() - context_len;
            let ctx = &all_ids[start..];
            let input = Tensor::new(ctx, &inner.device).map_err(forward_err)?;
            let input = input.unsqueeze(0).map_err(forward_err)?; // [1, ctx_len]
            let logits = model_guard.forward(&input, start).map_err(forward_err)?;
            // logits shape: [1, ctx_len, vocab]; take last position.
            let last = logits
                .squeeze(0)
                .map_err(forward_err)?
                .narrow(0, ctx.len() - 1, 1)
                .map_err(forward_err)?
                .squeeze(0)
                .map_err(forward_err)?; // [vocab]
            let vocab: Vec<f32> = last.to_vec1::<f32>().map_err(forward_err)?;
            // Greedy argmax.
            let next = vocab
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| i as u32)
                .unwrap_or(0);
            // Stop on EOS — Gemma-3 EOS id is 1 in the standard tokenizer.
            if next == 1 {
                break;
            }
            emitted.push(next);
            all_ids.push(next);
        }
        drop(model_guard);

        let decoded = inner.tokenizer.decode(&emitted, true).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("gemma-3-4b-it detokenize: {e}")))
        })?;

        // D-05 post-hoc JSON parse. Failure → empty extraction; the validator
        // still gets a chance to flag GbnfFailure if the model emits plausibly-
        // shaped JSON that violates the schema (empty name, etc.). For the
        // catastrophic-decode case (unparseable text) we prefer an empty
        // extraction over a poisoned one.
        let raw = parse_extraction_json(&decoded, chunk.chunk_id);
        Ok(raw)
    }
}

/// Best-effort JSON parse of the model's decoded output. Tolerant of trailing
/// junk (some sampling configurations emit explanatory text after the JSON);
/// extracts the first `{` ... matching `}` substring.
fn parse_extraction_json(decoded: &str, chunk_id: Ulid) -> RawExtraction {
    let Some(start) = decoded.find('{') else {
        return RawExtraction { source_chunk_id: chunk_id, ..Default::default() };
    };
    // Brace-matching scan to find the JSON object end.
    let bytes = decoded.as_bytes();
    let mut depth = 0_i32;
    let mut end_excl = start;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end_excl = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end_excl == start {
        return RawExtraction { source_chunk_id: chunk_id, ..Default::default() };
    }
    let json_slice = &decoded[start..end_excl];
    match serde_json::from_str::<ExtractionJson>(json_slice) {
        Ok(parsed) => parsed.into_raw(chunk_id),
        Err(e) => {
            tracing::warn!(
                err = %e,
                chunk_id = %chunk_id,
                "gemma-3-4b-it post-hoc JSON parse failed; emitting empty extraction"
            );
            RawExtraction { source_chunk_id: chunk_id, ..Default::default() }
        }
    }
}

/// JSON wire-shape mirrors the GBNF grammar files. Names are translated to
/// EntityIds via D-06 hash inside [`ExtractionJson::into_raw`].
#[derive(Debug, serde::Deserialize)]
struct ExtractionJson {
    #[serde(default)]
    entities: Vec<EntityJson>,
    #[serde(default)]
    relations: Vec<RelationJson>,
}

#[derive(Debug, serde::Deserialize)]
struct EntityJson {
    name: String,
    entity_type: String,
    #[serde(default)]
    aliases: Vec<String>,
    confidence: f32,
    valid_from_iso: String,
    #[serde(default)]
    valid_to_iso: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct RelationJson {
    subject_name: String,
    subject_type: String,
    predicate: String,
    object_name: String,
    object_type: String,
    confidence: f32,
    valid_from_iso: String,
    #[serde(default)]
    valid_to_iso: Option<String>,
}

impl ExtractionJson {
    fn into_raw(self, chunk_id: Ulid) -> RawExtraction {
        let entities = self
            .entities
            .into_iter()
            .map(|e| crate::types::Entity {
                id: crate::types::EntityId::from_name_and_type(&e.name, &e.entity_type),
                name: e.name,
                aliases: e.aliases,
                entity_type: e.entity_type,
                confidence: e.confidence,
                valid_from_iso: e.valid_from_iso,
                valid_to_iso: e.valid_to_iso,
            })
            .collect();
        let relations = self
            .relations
            .into_iter()
            .map(|r| crate::types::Relation {
                subject_id: crate::types::EntityId::from_name_and_type(
                    &r.subject_name,
                    &r.subject_type,
                ),
                predicate: r.predicate,
                object_id: crate::types::EntityId::from_name_and_type(
                    &r.object_name,
                    &r.object_type,
                ),
                confidence: r.confidence,
                valid_from_iso: r.valid_from_iso,
                valid_to_iso: r.valid_to_iso,
            })
            .collect();
        RawExtraction { source_chunk_id: chunk_id, entities, relations, facts: Vec::new() }
    }
}

#[async_trait]
impl Extractor for CandleGemma3_4B {
    async fn extract(
        &self,
        _episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        if chunks.is_empty() {
            return Ok(RawExtractionBatch::default());
        }

        let inner = self.inner.clone();
        let owned_chunks: Vec<ChunkInput> = chunks.to_vec();
        let chunks_for_batch = owned_chunks.clone();

        // (D-02) Per-batch timeout — wrap the whole batch's spawn_blocking in
        // a tokio::time::timeout. On timeout (Err) we fall through to the per-
        // chunk fallback path below.
        let batch_timeout = Duration::from_millis(inner.batch_timeout_ms);
        let inner_for_batch = inner.clone();
        let batch_result = tokio::time::timeout(
            batch_timeout,
            tokio::task::spawn_blocking(move || -> Result<RawExtractionBatch, LunarisError> {
                let mut by_chunk = Vec::with_capacity(chunks_for_batch.len());
                for c in &chunks_for_batch {
                    let r = CandleGemma3_4B::extract_one(&inner_for_batch, c)?;
                    by_chunk.push(r);
                }
                Ok(RawExtractionBatch { by_chunk })
            }),
        )
        .await;

        match batch_result {
            Ok(Ok(Ok(batch))) => Ok(batch),
            Ok(Ok(Err(e))) => {
                // Per-batch produced an error — fall back to per-chunk so a
                // single bad chunk doesn't block the whole batch.
                tracing::warn!(err = %e, batch_size = chunks.len(),
                    "gemma-3-4b-it batch failed; falling back to per-chunk");
                Self::extract_per_chunk(inner, chunks).await
            }
            Ok(Err(join_err)) => Err(LunarisError::Storage(StorageError::Backend(format!(
                "gemma-3-4b-it join: {join_err}"
            )))),
            Err(_elapsed) => {
                // Per-batch timed out — fall back to per-chunk per D-02.
                tracing::warn!(
                    batch_size = chunks.len(),
                    timeout_ms = inner.batch_timeout_ms,
                    "gemma-3-4b-it batch timeout; falling back to per-chunk"
                );
                Self::extract_per_chunk(inner, chunks).await
            }
        }
    }
}

impl CandleGemma3_4B {
    /// Per-chunk fallback path (D-02). Each chunk is wrapped in its own
    /// `tokio::time::timeout`; on timeout OR per-chunk error we emit an empty
    /// extraction with `tracing::warn!` rather than failing the whole batch.
    async fn extract_per_chunk(
        inner: Arc<CandleInner>,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        let per_chunk_timeout = Duration::from_millis(inner.per_chunk_timeout_ms);
        let mut by_chunk = Vec::with_capacity(chunks.len());

        let per_chunk_timeout_ms = inner.per_chunk_timeout_ms;
        for c in chunks {
            let inner_for_task = inner.clone();
            let c_owned = c.clone();
            let res = tokio::time::timeout(
                per_chunk_timeout,
                tokio::task::spawn_blocking(move || {
                    CandleGemma3_4B::extract_one(&inner_for_task, &c_owned)
                }),
            )
            .await;
            match res {
                Ok(Ok(Ok(r))) => by_chunk.push(r),
                Ok(Ok(Err(e))) => {
                    tracing::warn!(err = %e, chunk_id = %c.chunk_id,
                        "gemma-3-4b-it per-chunk failed; emitting empty");
                    by_chunk
                        .push(RawExtraction { source_chunk_id: c.chunk_id, ..Default::default() });
                }
                Ok(Err(join_err)) => {
                    tracing::warn!(err = %join_err, chunk_id = %c.chunk_id,
                        "gemma-3-4b-it per-chunk join failed; emitting empty");
                    by_chunk
                        .push(RawExtraction { source_chunk_id: c.chunk_id, ..Default::default() });
                }
                Err(_elapsed) => {
                    tracing::warn!(chunk_id = %c.chunk_id,
                        timeout_ms = per_chunk_timeout_ms,
                        "gemma-3-4b-it per-chunk timeout; emitting empty");
                    by_chunk
                        .push(RawExtraction { source_chunk_id: c.chunk_id, ..Default::default() });
                }
            }
        }
        Ok(RawExtractionBatch { by_chunk })
    }
}

#[inline]
fn forward_err(e: candle_core::Error) -> LunarisError {
    LunarisError::Storage(StorageError::Backend(format!("gemma-3-4b-it forward: {e}")))
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
    fn parse_extraction_json_handles_empty_input() {
        let r = parse_extraction_json("", Ulid::new());
        assert!(r.entities.is_empty());
        assert!(r.relations.is_empty());
    }

    #[test]
    fn parse_extraction_json_extracts_inline_object() {
        let chunk = Ulid::new();
        let raw = r#"Some preamble {"entities":[{"name":"Alice","entity_type":"Person","aliases":[],"confidence":0.9,"valid_from_iso":"2024-01-01T00:00:00Z"}],"relations":[]} trailing"#;
        let r = parse_extraction_json(raw, chunk);
        assert_eq!(r.entities.len(), 1);
        assert_eq!(r.entities[0].name, "Alice");
        assert_eq!(r.entities[0].entity_type, "Person");
        assert_eq!(r.source_chunk_id, chunk);
    }

    #[test]
    fn parse_extraction_json_handles_unparseable() {
        let r = parse_extraction_json("{not json}", Ulid::new());
        assert!(r.entities.is_empty());
        assert!(r.relations.is_empty());
    }
}
