//! `NativeQuantizedEmbedder` — `Embedder` impl backed by Q4_K_M GGUF
//! granite-r2 (scaffold).
//!
//! Mirrors the structure of `crate::embedder::NativeEmbedder`:
//! - `Arc<Inner>` cheap-clone handle
//! - tokenizer + model + device live inside `Inner`
//! - `Embedder::embed_batch` defers to `tokio::task::spawn_blocking`; no
//!   lock is held across `.await` (vacuous — no lock at all on the hot
//!   path; the model state is read-only after load).
//!
//! **Scaffold status (2026-05-14):** `open` succeeds and `dim()` returns
//! the right number, but `embed_batch` short-circuits to a
//! `NotImplemented`-mapped `LunarisError::Storage(Backend(..))` because
//! `QuantizedModernBert::pooled_forward` is a placeholder. The RED
//! integration test (`tests/quantized_equivalence.rs`) exists and will
//! fail until the next session lands the port — that's the point.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::Device;
use lunaris_core::{Embedder, LunarisError, StorageError};

use crate::GRANITE_R2_DIM;
use crate::config::{ConfigError, ModernBertConfig};
use crate::quantized_modernbert::{QuantizedError, QuantizedModernBert};
use crate::tokenizer::{GraniteTokenizer, TokenizerError};

/// Construction options for the quantized path. Mirrors
/// `NativeEmbedderOpts` but reads a `.gguf` file instead of a
/// safetensors blob.
///
/// The `config_path` is still required and refers to the SAME `config.json`
/// the FP16 path consumes — the GGUF metadata advertises some fields that
/// disagree with the HF config (notably `pooling_type`, see
/// `conversion-evidence.md` trap #1). We trust HF as ground truth.
#[derive(Clone, Debug)]
pub struct NativeQuantizedEmbedderOpts {
    /// Path to the `granite-r2-311m-Q4_K_M.gguf` produced by
    /// `scripts/spike-convert-granite-r2-to-gguf.sh`. SHA-256 is pinned in
    /// `crate::GRANITE_R2_GGUF_Q4_SHA256`.
    pub gguf_path: PathBuf,
    /// Path to the HF `tokenizer.json` (identical to the FP16 path's input).
    pub tokenizer_path: PathBuf,
    /// Path to the HF `config.json` (identical to the FP16 path's input).
    pub config_path: PathBuf,
    /// candle device. `Device::Cpu` is the only verified target until the
    /// hardware-optimization milestone wires Metal/CUDA.
    pub device: Device,
}

/// Errors raised during construction or the hot path.
#[derive(Debug, thiserror::Error)]
pub enum NativeQuantizedEmbedderError {
    #[error("config: {0}")]
    Config(#[from] ConfigError),

    #[error("tokenizer: {0}")]
    Tokenizer(#[from] TokenizerError),

    #[error("quantized: {0}")]
    Quantized(#[from] QuantizedError),

    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),
}

impl From<NativeQuantizedEmbedderError> for LunarisError {
    fn from(e: NativeQuantizedEmbedderError) -> Self {
        LunarisError::Storage(StorageError::Backend(format!(
            "lunaris-embed-native (quantized): {e}"
        )))
    }
}

/// Quantized granite-r2 embedder. Cheap to clone — the heavy state lives
/// behind an `Arc<Inner>`.
#[derive(Clone)]
pub struct NativeQuantizedEmbedder {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for NativeQuantizedEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeQuantizedEmbedder")
            .field("dim", &GRANITE_R2_DIM)
            .field("max_len", &self.inner.tokenizer.max_len())
            .field("sha256", &crate::GRANITE_R2_GGUF_Q4_SHA256)
            .finish()
    }
}

struct Inner {
    model: QuantizedModernBert,
    tokenizer: GraniteTokenizer,
}

impl NativeQuantizedEmbedder {
    /// Construct from on-disk artifacts. Synchronous I/O for the same
    /// reasons the FP16 path takes — error mapping stays straightforward
    /// and callers concerned about runtime stalls can `spawn_blocking`
    /// at their own boundary.
    pub fn open(opts: NativeQuantizedEmbedderOpts) -> Result<Self, NativeQuantizedEmbedderError> {
        // O-01-B — physical-core rayon pool init.
        crate::rayon_pool::ensure_physical_core_pool();

        // O-01-C/D — Device upgrade.
        let mut opts = opts;
        opts.device = crate::device_select::select_device(opts.device);

        let cfg = ModernBertConfig::try_from_json_path(&opts.config_path)?;
        let tokenizer = GraniteTokenizer::from_file(&opts.tokenizer_path, &cfg)?;
        let model = QuantizedModernBert::load(&opts.gguf_path, &cfg, &opts.device)?;

        tracing::info!(
            backend = "lunaris-embed-native (quantized)",
            model = "granite-embedding-311m-multilingual-r2",
            gguf = %opts.gguf_path.display(),
            sha256 = %crate::GRANITE_R2_GGUF_Q4_SHA256,
            "quantized embedder loaded"
        );

        // O-01-C — warm-up matmul.
        crate::device_select::warmup_device(&opts.device);

        Ok(Self { inner: Arc::new(Inner { model, tokenizer }) })
    }

    /// Synchronous embed path — for tests / direct callers that already
    /// have the inputs marshalled. The async trait method wraps this in
    /// `spawn_blocking`.
    pub fn embed_blocking(
        &self,
        inputs: &[&str],
    ) -> Result<Vec<Vec<f32>>, NativeQuantizedEmbedderError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let batch = self.inner.tokenizer.encode_batch(inputs, self.inner.model.device())?;
        let pooled = self.inner.model.pooled_forward(&batch.input_ids, &batch.attention_mask)?;
        let rows: Vec<Vec<f32>> = pooled.to_vec2::<f32>()?;
        Ok(rows)
    }
}

#[async_trait]
impl Embedder for NativeQuantizedEmbedder {
    fn dim(&self) -> usize {
        GRANITE_R2_DIM
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let owned: Vec<String> = inputs.iter().map(|s| (*s).to_string()).collect();
        let me = self.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>, LunarisError> {
            // O-01-E + activation-budget — re-chunk by BOTH the row-count
            // ceiling (`public_batch_size`) and the `rows × max_seq²` activation
            // footprint (`plan_batches`). Mirror of the FP16 path; this is the
            // guarantee that actually holds for long inputs — see
            // `embedder::plan_batches` (RAPTOR-summary OOM fix).
            let lens: Vec<usize> = owned.iter().map(|s| s.len()).collect();
            let sizes = crate::embedder::plan_batches(
                &lens,
                crate::embedder::public_batch_size(),
                crate::embedder::activation_budget(),
            );
            let mut out: Vec<Vec<f32>> = Vec::with_capacity(owned.len());
            let mut start = 0usize;
            for sz in sizes {
                let refs: Vec<&str> = owned[start..start + sz].iter().map(|s| s.as_str()).collect();
                let rows = me.embed_blocking(&refs).map_err(LunarisError::from)?;
                out.extend(rows);
                start += sz;
            }
            Ok(out)
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!(
                "lunaris-embed-native (quantized) join: {e}"
            )))
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn _dyn_compat(e: NativeQuantizedEmbedder) -> std::sync::Arc<dyn Embedder> {
        std::sync::Arc::new(e)
    }

    #[test]
    fn open_with_missing_config_fails_fast() {
        let opts = NativeQuantizedEmbedderOpts {
            gguf_path: PathBuf::from("/tmp/__lunaris_no_gguf.gguf"),
            tokenizer_path: PathBuf::from("/tmp/__lunaris_no_tokenizer.json"),
            config_path: PathBuf::from("/tmp/__lunaris_no_config.json"),
            device: Device::Cpu,
        };
        let err = NativeQuantizedEmbedder::open(opts).expect_err("missing files must fail");
        match err {
            NativeQuantizedEmbedderError::Config(_) => {}
            other => panic!("expected Config error, got: {other:?}"),
        }
    }
}
