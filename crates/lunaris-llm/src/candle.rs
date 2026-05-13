//! Candle [`LlmBackend`] — in-process Gemma-3 inference via candle 0.10's
//! typed `candle_transformers::models::gemma3::Model`.
//!
//! ## Scope
//!
//! Model-size-agnostic (4B / 27B / 270M). The caller picks the variant via
//! [`CandleBackendOpts::model_name`] + `model_path`; this module loads the
//! tokenizer + config + safetensors, runs a greedy decode loop on
//! `generate(prompt, ...)`, and returns the decoded string. Output-shape
//! enforcement (GBNF / JSON-schema) lives **in the prompt**, not the
//! sampler — candle 0.10's `LogitsProcessor` does not yet expose stable
//! grammar-constrained sampling, so the upstream caller is responsible
//! for post-hoc JSON parsing (mirrors the strategy currently used by
//! `lunaris-extract::candle_gemma3::parse_extraction_json`).
//!
//! ## CLAUDE.md compliance
//!
//! - `#![forbid(unsafe_code)]` upheld by `VarBuilder::from_buffered_safetensors`
//!   (safe alternative to `from_mmaped_safetensors`).
//! - Blocking work wrapped in `tokio::task::spawn_blocking`.
//! - `parking_lot::Mutex<Gemma3Model>` taken INSIDE `spawn_blocking` — never
//!   across `.await`.

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

use crate::{GenOpts, LlmBackend, SchemaConstraint};

/// Construction options for [`CandleBackend`].
#[derive(Clone, Debug)]
pub struct CandleBackendOpts {
    /// Logical name (used in [`LlmBackend::model_id`] telemetry). e.g.
    /// `"gemma-3-4b-it"`. Does NOT affect loading — `model_path` is the
    /// filesystem source of truth.
    pub model_name: String,
    /// Directory containing `tokenizer.json`, `config.json`, and
    /// `model.safetensors`. Required (no default — the caller already
    /// knows which model size it wants).
    pub model_path: PathBuf,
    /// candle compute device. v0 ships `Device::Cpu`.
    pub device: Device,
}

impl CandleBackendOpts {
    /// Convenience: build opts pointing at `~/.cache/lunaris/models/<name>/`.
    pub fn from_cache(model_name: impl Into<String>) -> Self {
        let name = model_name.into();
        let cache_root = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
        let model_path = cache_root.join("lunaris/models").join(&name);
        Self { model_name: name, model_path, device: Device::Cpu }
    }
}

/// In-process candle Gemma-3 backend. Cloneable (model handle is
/// `Arc`-shared); a single instance can be wired into extract, verify,
/// and reflect concurrently.
#[derive(Clone)]
pub struct CandleBackend {
    inner: Arc<CandleInner>,
}

impl std::fmt::Debug for CandleBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleBackend")
            .field("model_id", &self.inner.model_id)
            .field("device", &format_args!("{:?}", self.inner.device))
            .finish()
    }
}

struct CandleInner {
    model_id: String,
    tokenizer: Tokenizer,
    /// `Gemma3Model::forward(&mut self, ...)` needs `&mut`. Lock is taken
    /// inside `spawn_blocking` so the no-lock-across-await rule holds
    /// trivially.
    model: Mutex<Gemma3Model>,
    device: Device,
}

impl CandleBackend {
    /// Construct from explicit opts.
    pub async fn new(opts: CandleBackendOpts) -> Result<Self, LunarisError> {
        let model_name = opts.model_name.clone();
        let model_path = opts.model_path.clone();
        let device = opts.device.clone();

        // Fast-path cache-miss error: actionable, no spawn_blocking burn.
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
                    "{model_name} weights missing at {} (no {label}) — run `huggingface-cli download google/{model_name} --local-dir {}`",
                    model_path.display(),
                    model_path.display()
                ))));
            }
        }

        let model_id = format!("candle://{model_name}");
        let inner = tokio::task::spawn_blocking(move || -> Result<CandleInner, LunarisError> {
            let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!("{model_name} tokenizer: {e}")))
            })?;

            let cfg_bytes = std::fs::read(&config_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "{model_name} config: read {} ({e})",
                    config_path.display()
                )))
            })?;
            let cfg: Gemma3Config = serde_json::from_slice(&cfg_bytes).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "{model_name} config: parse {} ({e})",
                    config_path.display()
                )))
            })?;

            let bytes = std::fs::read(&safetensors_path).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "{model_name} weights: read {} ({e})",
                    safetensors_path.display()
                )))
            })?;
            let vb =
                VarBuilder::from_buffered_safetensors(bytes, DType::F32, &device).map_err(|e| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "{model_name} weights: {e}"
                    )))
                })?;

            // `use_flash_attn=false` on CPU per the existing Plan 02-03 +
            // Plan 03-01 patterns (Metal/CUDA enable separately upstream).
            let model = Gemma3Model::new(false, &cfg, vb).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "{model_name} model construct: {e}"
                )))
            })?;

            Ok(CandleInner { model_id, tokenizer, model: Mutex::new(model), device })
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("{} join: {e}", opts.model_name)))
        })??;

        Ok(Self { inner: Arc::new(inner) })
    }
}

#[async_trait]
impl LlmBackend for CandleBackend {
    async fn generate(
        &self,
        prompt: &str,
        constraint: SchemaConstraint<'_>,
        opts: GenOpts,
    ) -> Result<String, LunarisError> {
        // Constraint reminder: candle 0.10 has no stable grammar binding,
        // so any GBNF/JSON-schema lives in the prompt. Callers handle the
        // post-hoc parse.
        let inner = Arc::clone(&self.inner);
        // Materialize prompt + grammar text inside the spawned task. We
        // append the grammar to the prompt as a hint — same shape the
        // extract pipeline uses today.
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

        // Honour opts.timeout — match the per-batch budget the extract
        // pipeline (D-02) already enforces today.
        let result = match tokio::time::timeout(opts.timeout, fut).await {
            Ok(join_res) => join_res.map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!("{model_name} join: {e}")))
            })?,
            Err(_) => Err(LunarisError::Storage(StorageError::Backend(format!(
                "{model_name} timeout after {:?}",
                opts.timeout
            )))),
        };
        result
    }

    fn model_id(&self) -> &str {
        &self.inner.model_id
    }
}

fn forward_greedy(
    inner: &CandleInner,
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
        let logits =
            model_guard.forward(&input, start).map_err(|e| forward_err(&inner.model_id, e))?;
        let last = logits
            .squeeze(0)
            .map_err(|e| forward_err(&inner.model_id, e))?
            .narrow(0, ctx.len() - 1, 1)
            .map_err(|e| forward_err(&inner.model_id, e))?
            .squeeze(0)
            .map_err(|e| forward_err(&inner.model_id, e))?;
        let vocab: Vec<f32> = last.to_vec1::<f32>().map_err(|e| forward_err(&inner.model_id, e))?;
        let next = vocab
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i as u32)
            .unwrap_or(0);
        // Gemma-3 EOS id is 1 in the standard tokenizer.
        if next == 1 {
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

/// Convenience: a `GenOpts` matching the extract D-02 per-batch budget for
/// a 4B model. Verify (27B) needs a longer timeout — call sites should
/// override.
#[allow(dead_code)]
pub const GEN_OPTS_BATCH_4B: GenOpts =
    GenOpts { max_tokens: 512, temperature: 0.0, timeout: Duration::from_millis(150) };

#[cfg(test)]
mod tests {
    use super::*;

    /// Cache-miss returns the actionable error message (matches the
    /// existing `missing_weights_returns_actionable_error` test in
    /// lunaris-extract — fast, no model load).
    #[tokio::test]
    async fn missing_weights_returns_actionable_error() {
        let opts = CandleBackendOpts {
            model_name: "gemma-3-4b-it".into(),
            model_path: PathBuf::from("/this/path/definitely/does/not/exist"),
            device: Device::Cpu,
        };
        let err = CandleBackend::new(opts).await.expect_err("must error on missing path");
        let msg = err.to_string();
        assert!(
            msg.contains("gemma-3-4b-it weights missing"),
            "expected actionable cache-miss error, got: {msg}"
        );
        assert!(msg.contains("huggingface-cli download"), "expected hint, got: {msg}");
    }
}
