//! `NativeEmbedder` — `lunaris_core::Embedder` impl backed by candle's
//! ModernBERT for `ibm-granite/granite-embedding-311m-multilingual-r2`.
//!
//! See [`crate`] for the architectural rationale. This module:
//! - holds the `Arc<Inner>` cheap-clone handle,
//! - constructs the model from safetensors + `tokenizer.json` + `config.json`,
//! - implements `Embedder::embed_batch` via `tokio::task::spawn_blocking`
//!   with a `parking_lot::Mutex` scratch lock acquired INSIDE the closure
//!   (never across `.await` — CLAUDE.md lock discipline).
//!
//! ## Failure modes
//!
//! | Condition                              | Returned error                                       |
//! |----------------------------------------|------------------------------------------------------|
//! | safetensors file missing / unreadable  | `LunarisError::Storage(StorageError::Backend(..))`   |
//! | tokenizer.json missing / unreadable    | `LunarisError::Storage(StorageError::Backend(..))`   |
//! | config.json missing / invalid          | `LunarisError::Storage(StorageError::Backend(..))`   |
//! | candle forward pass failure            | `LunarisError::Storage(StorageError::Backend(..))`   |
//! | spawn_blocking join failure (panic)    | `LunarisError::Storage(StorageError::Backend(..))`   |
//!
//! Mutex is `parking_lot::Mutex` (poison-free) — no poison handling required.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::{DType, Device};
use candle_nn::VarBuilder;
use candle_transformers::models::modernbert::ModernBert;
use lunaris_core::{Embedder, LunarisError, StorageError};

use crate::GRANITE_R2_DIM;
use crate::config::{ConfigError, ModernBertConfig};
use crate::modernbert::{ForwardError, pooled_forward};
use crate::tokenizer::{EncodedBatch, GraniteTokenizer, TokenizerError};

/// Public construction options.
///
/// All three paths are required; defaults are not provided because there's no
/// canonical on-disk layout for granite-r2 (it lives wherever the operator
/// downloaded it). Tests use the env-var convention
/// `GRANITE_R2_WEIGHTS_PATH` / `GRANITE_R2_TOKENIZER_PATH` /
/// `GRANITE_R2_CONFIG_PATH` to point at the local cache.
#[derive(Clone, Debug)]
pub struct NativeEmbedderOpts {
    pub weights_path: PathBuf,
    pub tokenizer_path: PathBuf,
    pub config_path: PathBuf,
    pub device: Device,
}

/// Errors raised during construction. Hot-path errors are mapped to
/// `LunarisError` via the `Embedder` trait surface; this enum exists so
/// `NativeEmbedder::open` can surface load-time problems with structure
/// (callers can inspect / log specific variants before falling back).
#[derive(Debug, thiserror::Error)]
pub enum NativeEmbedderError {
    #[error("config: {0}")]
    Config(#[from] ConfigError),

    #[error("tokenizer: {0}")]
    Tokenizer(#[from] TokenizerError),

    #[error("weights: {0}")]
    Weights(String),

    #[error("forward: {0}")]
    Forward(#[from] ForwardError),

    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),
}

impl From<NativeEmbedderError> for LunarisError {
    fn from(e: NativeEmbedderError) -> Self {
        LunarisError::Storage(StorageError::Backend(format!("lunaris-embed-native: {e}")))
    }
}

/// Granite-r2 native embedder. Cheap to clone — the heavy state lives behind
/// an `Arc<Inner>`.
#[derive(Clone)]
pub struct NativeEmbedder {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for NativeEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeEmbedder")
            .field("dim", &GRANITE_R2_DIM)
            .field("max_len", &self.inner.tokenizer.max_len())
            .finish()
    }
}

struct Inner {
    /// The forward-pass model. candle's `ModernBert::forward` takes `&self`
    /// and the heavy state (rotary caches, layer weights) is internally
    /// `Arc`-shared, so we don't need a `Mutex<ModernBert>` — concurrent
    /// `embed_batch` calls run truly in parallel (each on a `spawn_blocking`
    /// thread). CLAUDE.md lock-across-await constraint is satisfied
    /// vacuously: no lock is taken on the hot path.
    model: ModernBert,
    tokenizer: GraniteTokenizer,
    device: Device,
}

impl NativeEmbedder {
    /// Construct from on-disk artifacts. Synchronous I/O happens inline
    /// (reads safetensors header, parses tokenizer, loads weights into the
    /// chosen `Device`). Callers concerned about runtime stalls should wrap
    /// in `tokio::task::spawn_blocking`; we deliberately do NOT wrap inside
    /// `open` so error mapping stays straightforward.
    pub fn open(opts: NativeEmbedderOpts) -> Result<Self, NativeEmbedderError> {
        let cfg = ModernBertConfig::try_from_json_path(&opts.config_path)?;
        let tokenizer = GraniteTokenizer::from_file(&opts.tokenizer_path, &cfg)?;

        tracing::info!(
            backend = "lunaris-embed-native",
            model = "granite-embedding-311m-multilingual-r2",
            weights = %opts.weights_path.display(),
            "native embedder loading"
        );

        // Build the candle VarBuilder reading the safetensors file. Compute
        // in FP32 regardless of on-disk dtype — see modernbert.rs rationale.
        //
        // Weight-key rename: candle-transformers' ModernBert::load requests
        // keys with a `model.` prefix (e.g. `model.layers.0.attn.Wqkv.weight`)
        // but granite-r2's safetensors store the same tensors without the
        // prefix (`layers.0.attn.Wqkv.weight`). Strip the prefix on lookup
        // via `VarBuilder::rename_f`. The encoder-only path doesn't touch
        // `decoder.*` / `head.*` (those live on the SequenceClassification +
        // MaskedLM heads) so a blanket `strip_prefix("model.")` is sufficient.
        let vb = candle_safetensors_varbuilder(&opts.weights_path, &opts.device)?
            .rename_f(|name: &str| name.strip_prefix("model.").unwrap_or(name).to_string());
        let candle_cfg = cfg.to_candle();
        let model = ModernBert::load(vb, &candle_cfg)?;

        tracing::info!(
            backend = "lunaris-embed-native",
            model = "granite-embedding-311m-multilingual-r2",
            "native embedder initialized"
        );

        Ok(Self {
            inner: Arc::new(Inner { model, tokenizer, device: opts.device }),
        })
    }

    /// Synchronous embed path — for tests / direct callers that already have
    /// the inputs marshalled. The async trait method wraps this in
    /// `spawn_blocking`.
    pub fn embed_blocking(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, NativeEmbedderError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let EncodedBatch { input_ids, attention_mask } =
            self.inner.tokenizer.encode_batch(inputs, &self.inner.device)?;
        let pooled = pooled_forward(&self.inner.model, &input_ids, &attention_mask)?;
        let rows: Vec<Vec<f32>> = pooled.to_vec2::<f32>()?;
        for row in &rows {
            if row.len() != GRANITE_R2_DIM {
                return Err(NativeEmbedderError::Weights(format!(
                    "row width {} != GRANITE_R2_DIM ({})",
                    row.len(),
                    GRANITE_R2_DIM
                )));
            }
        }
        Ok(rows)
    }
}

/// Build a VarBuilder reading safetensors from disk in FP32 via a buffered
/// (non-mmap) loader. CLAUDE.md `#![forbid(unsafe_code)]` rules out
/// `VarBuilder::from_mmaped_safetensors` (which is `unsafe fn`); the
/// `from_buffered_safetensors` path is safe — it reads bytes into a `Vec`
/// up front, then candle decodes tensors lazily as `VarBuilder::get` is
/// called.
///
/// Trade-off: ~623 MB transient RSS during load (granite-r2 file size).
/// For a 311M-param model this is acceptable for the v0.4 milestone; the
/// FP32 compute dtype means steady-state RSS is ~1.24 GB regardless, so the
/// load-time spike is a small fraction of the total. The Q4 follow-up will
/// reconsider both knobs together.
///
/// Why FP32 compute? See `modernbert.rs` — the masked mean-pool needs fp32
/// accumulation to stay inside the 0.5% drift gate.
fn candle_safetensors_varbuilder(
    path: &std::path::Path,
    device: &Device,
) -> Result<VarBuilder<'static>, NativeEmbedderError> {
    let bytes = std::fs::read(path).map_err(|e| {
        NativeEmbedderError::Weights(format!(
            "safetensors read from {} failed: {e}",
            path.display()
        ))
    })?;
    VarBuilder::from_buffered_safetensors(bytes, DType::F32, device).map_err(|e| {
        NativeEmbedderError::Weights(format!(
            "safetensors decode from {} failed: {e}",
            path.display()
        ))
    })
}

#[async_trait]
impl Embedder for NativeEmbedder {
    fn dim(&self) -> usize {
        GRANITE_R2_DIM
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        // Move owned inputs across the blocking boundary; `&str` borrows are
        // not `'static`.
        let owned: Vec<String> = inputs.iter().map(|s| (*s).to_string()).collect();
        let me = self.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>, LunarisError> {
            let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
            me.embed_blocking(&refs).map_err(LunarisError::from)
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("lunaris-embed-native join: {e}")))
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trait dyn-compat: `NativeEmbedder` MUST be usable as `Arc<dyn Embedder>`
    // (the public Lunaris construction pattern). This compiles-only test
    // proves the trait object is well-formed without needing real weights.
    #[allow(dead_code)]
    fn _dyn_compat(e: NativeEmbedder) -> std::sync::Arc<dyn Embedder> {
        std::sync::Arc::new(e)
    }

    #[test]
    fn open_with_missing_config_fails_fast() {
        let opts = NativeEmbedderOpts {
            weights_path: PathBuf::from("/tmp/__lunaris_no_weights.safetensors"),
            tokenizer_path: PathBuf::from("/tmp/__lunaris_no_tokenizer.json"),
            config_path: PathBuf::from("/tmp/__lunaris_no_config.json"),
            device: Device::Cpu,
        };
        let err = NativeEmbedder::open(opts).expect_err("missing files must fail");
        // Config is loaded first → error variant is Config.
        match err {
            NativeEmbedderError::Config(_) => {}
            other => panic!("expected Config error, got: {other:?}"),
        }
    }
}
