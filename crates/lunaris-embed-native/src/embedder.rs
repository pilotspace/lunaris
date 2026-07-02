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

/// O-01-E — Public-API batch ceiling. User-supplied batches larger than this
/// are re-chunked into windows of `MAX_PUBLIC_BATCH` before dispatch.
///
/// **Why 8 and not larger:** HARDWARE-OPTIMIZATION-ROADMAP §57 — "avoids
/// activation-tensor OOM on Metal's smaller memory ceiling." On a unified-
/// memory 8 GB M-series, granite-r2 FP32 weights take ~1.24 GB and each
/// batch element's intermediate activations at max seq=512 are ~12 MB across
/// the alternating-attention stack. A batch of 8 caps activation scratch at
/// ~96 MB; doubling to 16 starts contending with the OS framebuffer at
/// fragmentation-sensitive moments. CPU + CUDA can easily push higher but
/// the public API guarantees the smallest acceptable ceiling so a single
/// codepath works on every backend.
///
/// Operators who explicitly want larger batches must call `embed_blocking`
/// directly (it does NOT re-chunk).
pub const MAX_PUBLIC_BATCH: usize = 8;

/// Pure resolver for the effective re-chunk ceiling from a raw env value.
/// Split out from [`public_batch_size`] so the fallback policy is unit-testable
/// without touching process env (avoids the global-env test race).
///
/// Design-for-failure: a missing, non-numeric, or zero value yields the safe
/// default [`MAX_PUBLIC_BATCH`] rather than panicking or producing a 0-window
/// (`slice::chunks(0)` panics).
fn resolve_batch_size(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(MAX_PUBLIC_BATCH)
}

/// Effective per-forward re-chunk ceiling. Defaults to [`MAX_PUBLIC_BATCH`] (8,
/// the safe cross-backend activation-footprint ceiling) but operators on
/// memory-rich hosts (Apple M-Pro/Max, CUDA) can raise it via the
/// `LUNARIS_EMBED_BATCH` env var to better saturate the GPU on bulk ingest —
/// e.g. the LongMemEval haystack ingest that embeds hundreds of chunks per
/// document. Read once and cached for the process lifetime.
pub fn public_batch_size() -> usize {
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| resolve_batch_size(std::env::var("LUNARIS_EMBED_BATCH").ok().as_deref()))
}
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
        // O-01-B — install rayon's global thread pool at physical-core count
        // before any candle CPU op fires. No-op on `Device::Metal` /
        // `Device::Cuda` (those backends schedule their own kernels), but
        // safe to call regardless — idempotent.
        crate::rayon_pool::ensure_physical_core_pool();

        // O-01-C/D — upgrade `Device::Cpu` to Metal/CUDA when the matching
        // feature is on and the GPU init succeeds. Caller-supplied non-Cpu
        // devices are honored verbatim.
        let mut opts = opts;
        opts.device = crate::device_select::select_device(opts.device);

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

        // O-01-C — pay GPU JIT / kernel-cache cost up front on the selected
        // device so the first user query doesn't eat the spike. Best-effort
        // (errors are logged + swallowed; the real forward pass will surface
        // any persistent device issue with proper context).
        crate::device_select::warmup_device(&opts.device);

        Ok(Self { inner: Arc::new(Inner { model, tokenizer, device: opts.device }) })
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
            // O-01-E — public-API batch ceiling. Re-chunk user input into
            // windows of MAX_PUBLIC_BATCH so the activation-tensor footprint
            // is bounded regardless of caller batch size. Each chunk runs
            // through the same `embed_blocking` synchronous path; we
            // concatenate the per-chunk rows preserving input order. For
            // batches ≤ MAX_PUBLIC_BATCH this is a single chunk = single
            // forward pass = zero overhead vs the pre-O-01-E path.
            let batch = public_batch_size();
            let mut out: Vec<Vec<f32>> = Vec::with_capacity(owned.len());
            for chunk in owned.chunks(batch) {
                let refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
                let rows = me.embed_blocking(&refs).map_err(LunarisError::from)?;
                out.extend(rows);
            }
            Ok(out)
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

    // O-01-E+ — LUNARIS_EMBED_BATCH override resolver. Pure-function tests so
    // the fallback policy is verified without mutating process env.
    #[test]
    fn resolve_batch_size_defaults_when_unset() {
        assert_eq!(resolve_batch_size(None), MAX_PUBLIC_BATCH);
    }

    #[test]
    fn resolve_batch_size_honors_large_override() {
        assert_eq!(resolve_batch_size(Some("64")), 64);
        assert_eq!(resolve_batch_size(Some("128")), 128);
    }

    #[test]
    fn resolve_batch_size_trims_whitespace() {
        assert_eq!(resolve_batch_size(Some("  32 ")), 32);
    }

    #[test]
    fn resolve_batch_size_rejects_zero_and_garbage() {
        // 0 would make `slice::chunks(0)` panic — must fall back, never 0.
        assert_eq!(resolve_batch_size(Some("0")), MAX_PUBLIC_BATCH);
        assert_eq!(resolve_batch_size(Some("-4")), MAX_PUBLIC_BATCH);
        assert_eq!(resolve_batch_size(Some("abc")), MAX_PUBLIC_BATCH);
        assert_eq!(resolve_batch_size(Some("")), MAX_PUBLIC_BATCH);
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

    /// O-01-E — `MAX_PUBLIC_BATCH` is the cross-backend activation-footprint
    /// ceiling. Locking it to 8 is a public-API contract; bumping it requires
    /// re-measuring Metal activation OOM on the smallest supported host
    /// (HARDWARE-OPTIMIZATION-ROADMAP §57). A future change that lifts the
    /// ceiling without updating this test is the canary.
    #[test]
    fn max_public_batch_is_eight() {
        assert_eq!(MAX_PUBLIC_BATCH, 8);
    }

    /// `chunks(MAX_PUBLIC_BATCH)` over an input of N produces ceil(N/8)
    /// chunks whose concatenation preserves order. The async `embed_batch`
    /// relies on this property to keep output indices aligned with input
    /// indices — verify the std-lib contract here so a future swap to a
    /// custom chunker doesn't silently break ordering.
    #[test]
    fn chunking_preserves_order_and_size() {
        let xs: Vec<i32> = (0..21).collect();
        let chunks: Vec<&[i32]> = xs.chunks(MAX_PUBLIC_BATCH).collect();
        assert_eq!(chunks.len(), 3); // ceil(21 / 8)
        assert_eq!(chunks[0].len(), 8);
        assert_eq!(chunks[1].len(), 8);
        assert_eq!(chunks[2].len(), 5);
        let flat: Vec<i32> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
        assert_eq!(flat, xs);
    }
}
