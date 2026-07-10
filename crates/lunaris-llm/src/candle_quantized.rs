//! Candle [`LlmBackend`] — Q4 GGUF Gemma-3 inference via candle-transformers'
//! typed `candle_transformers::models::quantized_gemma3::ModelWeights`.
//!
//! ## Scope
//!
//! Mirrors [`crate::candle::CandleBackend`] (the F32 path) but loads weights
//! from a single-file GGUF instead of `model.safetensors`, avoiding the
//! ~16 GB F32 materialization. `ModelWeights::from_gguf` derives every
//! architecture parameter (head counts, hidden size, RoPE frequencies,
//! sliding-window pattern, ...) directly from the GGUF's own metadata — we
//! do **not** re-parse `config.json` into `candle_transformers::models::
//! gemma3::Config` for that purpose (there is nothing to reconcile: GGUF is
//! authoritative for shapes because it's what was actually converted+loaded).
//!
//! What GGUF metadata is NOT trusted for: **generation-affecting semantics
//! that the HF export already pins**, specifically the EOS token id(s) that
//! bound the greedy decode loop. This mirrors the reranker's `pooling_type`
//! lesson (`lunaris-rerank-native::quantized_xlmr` module docs) — llama.cpp
//! GGUF converters do carry a `tokenizer.ggml.eos_token_id` key, but chat
//! fine-tunes (like `gemma-3-4b-it`) route their real end-of-turn signal
//! through the tokenizer's `<end_of_turn>` id documented in the HF
//! `config.json`'s top-level `eos_token_id` field — not always `1`
//! (`<eos>`, the base model's stop token). We read that field straight from
//! the HF `config.json` instead of trusting whatever the converter baked
//! into the GGUF.
//!
//! ## Decode loop parity with `CandleBackend`
//!
//! `quantized_gemma3::ModelWeights::forward(&mut self, x: &Tensor,
//! index_pos: usize) -> Result<Tensor>` has the *exact* same
//! `(context_tensor, absolute_position)` signature as the F32
//! `gemma3::Model::forward` that [`crate::candle::CandleBackend`] already
//! drives — both maintain an internal per-layer KV cache keyed by
//! `index_pos`. The greedy loop below is therefore structurally identical
//! to `crate::candle::forward_greedy`, with two differences: (1) the
//! quantized `forward` already narrows to the last position internally
//! (returns `(batch, vocab)`, not `(batch, seq, vocab)` — no extra
//! `narrow` needed), and (2) the stop condition checks a resolved
//! `Vec<u32>` of EOS ids instead of a hardcoded `1`.
//!
//! ## CLAUDE.md compliance
//!
//! - `#![forbid(unsafe_code)]` (crate-level) — GGUF loading + quantized
//!   matmul go through candle's safe API surface exclusively.
//! - Blocking work (file I/O + GGUF parse + `ModelWeights::from_gguf` + the
//!   decode loop) wrapped in `tokio::task::spawn_blocking`, mirroring
//!   `CandleBackend::new` / `generate`.
//! - `parking_lot::Mutex<ModelWeights>` taken INSIDE `spawn_blocking` — never
//!   across `.await`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::models::quantized_gemma3::ModelWeights;
use lunaris_core::{LunarisError, StorageError};
use parking_lot::Mutex;
use tokenizers::Tokenizer;

use crate::{GenOpts, LlmBackend, SchemaConstraint};

/// Standard Gemma `<eos>` token id — the base model's stop token. Used as
/// the last-resort fallback when the HF `config.json` is missing/
/// unparseable/lacks `eos_token_id` (see module docs on why GGUF metadata
/// is not trusted for this instead).
const DEFAULT_EOS_TOKEN_ID: u32 = 1;

/// Construction options for [`QuantizedCandleBackend`].
#[derive(Clone, Debug)]
pub struct QuantizedCandleBackendOpts {
    /// Logical name (used in [`LlmBackend::model_id`] telemetry), e.g.
    /// `"gemma-3-4b-it"`. Does NOT affect loading.
    pub model_name: String,
    /// Path to the single-file Q4 GGUF (e.g. `gemma-3-4b-it-q4_0.gguf` from
    /// `google/gemma-3-4b-it-qat-q4_0-gguf`).
    pub gguf_path: PathBuf,
    /// Path to the HF `tokenizer.json` (source of truth for tokenization —
    /// GGUF metadata is not consulted for this).
    pub tokenizer_path: PathBuf,
    /// Path to the HF `config.json` — consulted ONLY for the top-level
    /// `eos_token_id` field (see module docs). Falls back to
    /// [`DEFAULT_EOS_TOKEN_ID`] with a `tracing::warn!` if missing,
    /// unparseable, or empty.
    pub config_path: PathBuf,
    /// candle compute device. `Device::Cpu` is upgraded to Metal/Cuda when
    /// the corresponding feature is enabled — see [`select_device`]. A
    /// caller-supplied non-`Cpu` device is honored verbatim.
    pub device: Device,
}

/// Q4 GGUF Gemma-3 backend. Cloneable (model handle is `Arc`-shared); a
/// single instance can be wired into extract, verify, and reflect
/// concurrently, same as [`crate::candle::CandleBackend`].
#[derive(Clone)]
pub struct QuantizedCandleBackend {
    inner: Arc<QuantizedInner>,
}

impl std::fmt::Debug for QuantizedCandleBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuantizedCandleBackend")
            .field("model_id", &self.inner.model_id)
            .field("device", &format_args!("{:?}", self.inner.device))
            .field("stop_ids", &self.inner.stop_ids)
            .finish()
    }
}

struct QuantizedInner {
    model_id: String,
    tokenizer: Tokenizer,
    /// `ModelWeights::forward(&mut self, ...)` needs `&mut`. Lock is taken
    /// inside `spawn_blocking` so the no-lock-across-await rule holds
    /// trivially (mirrors `CandleBackend`'s `CandleInner::model`).
    model: Mutex<ModelWeights>,
    device: Device,
    /// EOS token id(s) that terminate the greedy decode loop. Resolved from
    /// the HF `config.json`, NOT the GGUF metadata (module docs).
    stop_ids: Vec<u32>,
}

impl QuantizedCandleBackend {
    /// Construct from explicit opts.
    pub async fn new(opts: QuantizedCandleBackendOpts) -> Result<Self, LunarisError> {
        let model_name = opts.model_name.clone();
        let gguf_path = opts.gguf_path.clone();
        let tokenizer_path = opts.tokenizer_path.clone();
        let config_path = opts.config_path.clone();
        let device = select_device(opts.device.clone());

        // Fast-path cache-miss error: actionable, no spawn_blocking burn.
        // Mirrors CandleBackend::new's three-file existence check.
        for (label, p) in [
            ("gguf", &gguf_path),
            ("tokenizer.json", &tokenizer_path),
            ("config.json", &config_path),
        ] {
            if !p.exists() {
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "{model_name} quantized weights missing at {} (no {label}) — stage the Q4 \
                     GGUF (e.g. `huggingface-cli download google/gemma-3-4b-it-qat-q4_0-gguf \
                     --local-dir <gguf_dir>`) and the HF tokenizer/config (e.g. \
                     `huggingface-cli download google/gemma-3-4b-it --include tokenizer.json,config.json \
                     --local-dir <hf_dir>`)",
                    p.display()
                ))));
            }
        }

        warmup_device(&device);

        let model_id = format!("candle-gguf://{model_name}");
        let model_name_for_closure = model_name.clone();
        let inner = tokio::task::spawn_blocking(move || -> Result<QuantizedInner, LunarisError> {
            let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "{model_name_for_closure} tokenizer: {e}"
                )))
            })?;

            let stop_ids = load_eos_ids(&config_path, &model_name_for_closure);

            let mut file = std::fs::File::open(&gguf_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "{model_name_for_closure} gguf: open {} ({e})",
                    gguf_path.display()
                )))
            })?;
            let content = gguf_file::Content::read(&mut file).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "{model_name_for_closure} gguf: read header {} ({e})",
                    gguf_path.display()
                )))
            })?;
            let model = ModelWeights::from_gguf(content, &mut file, &device).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "{model_name_for_closure} gguf: model construct {e}"
                )))
            })?;

            Ok(QuantizedInner { model_id, tokenizer, model: Mutex::new(model), device, stop_ids })
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("{model_name} join: {e}")))
        })??;

        Ok(Self { inner: Arc::new(inner) })
    }
}

#[async_trait]
impl LlmBackend for QuantizedCandleBackend {
    async fn generate(
        &self,
        prompt: &str,
        constraint: SchemaConstraint<'_>,
        opts: GenOpts,
    ) -> Result<String, LunarisError> {
        // Same grammar/schema-in-prompt strategy as CandleBackend (candle
        // 0.10 has no stable grammar binding for the quantized path either).
        let inner = Arc::clone(&self.inner);
        let prompt = match constraint {
            SchemaConstraint::None => prompt.to_string(),
            SchemaConstraint::Gbnf(g) => format!("{prompt}\n\n[Output must match grammar]\n{g}"),
            SchemaConstraint::JsonSchema(schema) => {
                let sj = serde_json::to_string(schema).unwrap_or_else(|_| "{}".into());
                format!("{prompt}\n\n[Output must be JSON conforming to]\n{sj}")
            }
        };
        let max_new_tokens = opts.max_tokens as usize;
        let model_name = inner.model_id.clone();

        let fut = tokio::task::spawn_blocking(move || -> Result<String, LunarisError> {
            forward_greedy(&inner, &prompt, max_new_tokens)
        });

        match tokio::time::timeout(opts.timeout, fut).await {
            Ok(join_res) => join_res.map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!("{model_name} join: {e}")))
            })?,
            Err(_) => Err(LunarisError::Storage(StorageError::Backend(format!(
                "{model_name} timeout after {:?}",
                opts.timeout
            )))),
        }
    }

    fn model_id(&self) -> &str {
        &self.inner.model_id
    }
}

fn forward_greedy(
    inner: &QuantizedInner,
    prompt: &str,
    max_new_tokens: usize,
) -> Result<String, LunarisError> {
    let encoding = inner.tokenizer.encode(prompt, true).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("{} tokenize: {e}", inner.model_id)))
    })?;
    let mut all_ids: Vec<u32> = encoding.get_ids().to_vec();
    if all_ids.is_empty() {
        return Ok(String::new());
    }

    let mut model_guard = inner.model.lock();
    let mut emitted: Vec<u32> = Vec::with_capacity(max_new_tokens);

    for step in 0..max_new_tokens {
        let context_len = if step == 0 { all_ids.len() } else { 1 };
        let start = all_ids.len() - context_len;
        let ctx = &all_ids[start..];
        let input = Tensor::new(ctx, &inner.device).map_err(|e| forward_err(&inner.model_id, e))?;
        let input = input.unsqueeze(0).map_err(|e| forward_err(&inner.model_id, e))?;
        // `ModelWeights::forward` already narrows to the last position
        // internally — returns `(batch=1, vocab)`, unlike the F32 path's
        // `(batch, seq, vocab)` which CandleBackend narrows itself.
        let logits =
            model_guard.forward(&input, start).map_err(|e| forward_err(&inner.model_id, e))?;
        let last = logits.squeeze(0).map_err(|e| forward_err(&inner.model_id, e))?;
        let vocab: Vec<f32> = last.to_vec1::<f32>().map_err(|e| forward_err(&inner.model_id, e))?;
        let next = vocab
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i as u32)
            .unwrap_or(0);
        if inner.stop_ids.contains(&next) {
            break;
        }
        emitted.push(next);
        all_ids.push(next);
    }
    drop(model_guard);

    inner.tokenizer.decode(&emitted, true).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("{} detokenize: {e}", inner.model_id)))
    })
}

fn forward_err(model_id: &str, e: candle_core::Error) -> LunarisError {
    LunarisError::Storage(StorageError::Backend(format!("{model_id} forward: {e}")))
}

/// Resolve the EOS token id(s) that terminate the greedy decode loop from
/// the HF `config.json`'s top-level `eos_token_id` field (an integer OR an
/// array of integers — chat fine-tunes commonly ship both `<eos>` and
/// `<end_of_turn>`). Never fails construction: any read/parse problem
/// degrades to `[DEFAULT_EOS_TOKEN_ID]` with a `tracing::warn!` (the
/// existence pre-flight in `new` already guarantees the file is present;
/// this only guards against malformed *content*).
fn load_eos_ids(config_path: &Path, model_name: &str) -> Vec<u32> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum EosField {
        One(u32),
        Many(Vec<u32>),
    }

    let fallback = || {
        tracing::warn!(
            model = model_name,
            config = %config_path.display(),
            default_eos = DEFAULT_EOS_TOKEN_ID,
            "config.json eos_token_id missing/unparseable/empty; using the default Gemma <eos> id"
        );
        vec![DEFAULT_EOS_TOKEN_ID]
    };

    let bytes = match std::fs::read(config_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(model = model_name, error = %e, "config.json read failed; using default EOS id");
            return vec![DEFAULT_EOS_TOKEN_ID];
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(model = model_name, error = %e, "config.json parse failed; using default EOS id");
            return vec![DEFAULT_EOS_TOKEN_ID];
        }
    };
    match value.get("eos_token_id") {
        Some(field) => match serde_json::from_value::<EosField>(field.clone()) {
            Ok(EosField::One(id)) => vec![id],
            Ok(EosField::Many(ids)) if !ids.is_empty() => ids,
            Ok(EosField::Many(_)) | Err(_) => fallback(),
        },
        None => fallback(),
    }
}

/// Upgrade `Device::Cpu` to Metal/Cuda when the corresponding feature is
/// enabled; a caller-supplied non-`Cpu` device is honored verbatim. Mirrors
/// `lunaris-rerank-native::device_select::select_device` exactly (same
/// rationale — see that module for the full write-up).
fn select_device(requested: Device) -> Device {
    if !matches!(requested, Device::Cpu) {
        tracing::debug!(?requested, "select_device: caller-provided device honored verbatim");
        return requested;
    }

    #[cfg(feature = "cuda")]
    {
        match Device::new_cuda(0) {
            Ok(d) => {
                tracing::info!(
                    backend = "lunaris-llm (quantized)",
                    "select_device: Cpu → Cuda(0) (cuda feature on)"
                );
                return d;
            }
            Err(e) => {
                tracing::debug!(error = %e, "select_device: cuda init failed");
            }
        }
    }

    #[cfg(feature = "metal")]
    {
        match Device::new_metal(0) {
            Ok(d) => {
                tracing::info!(
                    backend = "lunaris-llm (quantized)",
                    "select_device: Cpu → Metal(0) (metal feature on)"
                );
                return d;
            }
            Err(e) => {
                tracing::debug!(error = %e, "select_device: metal init failed");
            }
        }
    }

    Device::Cpu
}

/// Tiny dummy matmul to pay GPU JIT cost up front. Best-effort; errors are
/// swallowed (the real GGUF forward pass will surface them with context).
/// Mirrors `lunaris-rerank-native::device_select::warmup_device`.
fn warmup_device(device: &Device) {
    let res: Result<(), candle_core::Error> = (|| {
        let a = Tensor::zeros((4, 4), candle_core::DType::F32, device)?;
        let b = Tensor::zeros((4, 4), candle_core::DType::F32, device)?;
        let _c = a.matmul(&b)?;
        Ok(())
    })();

    if let Err(e) = res {
        tracing::warn!(?device, error = %e, "quantized device warm-up matmul failed (best-effort)");
    } else {
        tracing::info!(?device, "quantized device warm-up matmul completed");
    }
}

/// Convenience: a `GenOpts` matching the extractor's D-02 per-batch budget.
/// The 150ms production default is unwinnable for CPU Q4 decode of a 4B
/// model (see `docs/design/quantized-inference-extractor-reranker.md` §3) —
/// call sites driving real decode MUST override `timeout` upward until the
/// §5 `extractor_decode` bench re-derives a realistic default.
#[allow(dead_code)]
pub const GEN_OPTS_BATCH_4B_GGUF: GenOpts =
    GenOpts { max_tokens: 512, temperature: 0.0, timeout: Duration::from_millis(150) };

#[cfg(test)]
mod tests {
    use super::*;

    /// Cache-miss returns an actionable error naming the missing artifact —
    /// mirrors `CandleBackend`'s `missing_weights_returns_actionable_error`.
    #[tokio::test]
    async fn missing_gguf_returns_actionable_error() {
        let opts = QuantizedCandleBackendOpts {
            model_name: "gemma-3-4b-it".into(),
            gguf_path: PathBuf::from("/this/path/definitely/does/not/exist.gguf"),
            tokenizer_path: PathBuf::from("/this/path/definitely/does/not/exist/tokenizer.json"),
            config_path: PathBuf::from("/this/path/definitely/does/not/exist/config.json"),
            device: Device::Cpu,
        };
        let err = QuantizedCandleBackend::new(opts).await.expect_err("must error on missing gguf");
        let msg = err.to_string();
        assert!(
            msg.contains("quantized weights missing"),
            "expected actionable cache-miss error, got: {msg}"
        );
        assert!(msg.contains("gguf"), "expected the error to name the missing gguf, got: {msg}");
    }

    #[test]
    fn select_device_does_not_panic() {
        let _ = select_device(Device::Cpu);
    }

    #[test]
    fn warmup_cpu_is_infallible() {
        warmup_device(&Device::Cpu);
    }

    /// Pure unit coverage of the EOS-resolution ladder without touching the
    /// filesystem existence pre-flight in `new` — writes throwaway
    /// `config.json` fixtures to a tempdir.
    #[test]
    fn load_eos_ids_reads_single_integer() {
        let dir =
            std::env::temp_dir().join(format!("lunaris_test_eos_single_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"eos_token_id": 106}"#).unwrap();
        let ids = load_eos_ids(&path, "test-model");
        assert_eq!(ids, vec![106]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_eos_ids_reads_array() {
        let dir =
            std::env::temp_dir().join(format!("lunaris_test_eos_array_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"eos_token_id": [1, 106]}"#).unwrap();
        let ids = load_eos_ids(&path, "test-model");
        assert_eq!(ids, vec![1, 106]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_eos_ids_falls_back_on_missing_field() {
        let dir =
            std::env::temp_dir().join(format!("lunaris_test_eos_missing_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"some_other_field": true}"#).unwrap();
        let ids = load_eos_ids(&path, "test-model");
        assert_eq!(ids, vec![DEFAULT_EOS_TOKEN_ID]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_eos_ids_falls_back_on_missing_file() {
        let ids =
            load_eos_ids(Path::new("/this/path/definitely/does/not/exist.json"), "test-model");
        assert_eq!(ids, vec![DEFAULT_EOS_TOKEN_ID]);
    }
}
