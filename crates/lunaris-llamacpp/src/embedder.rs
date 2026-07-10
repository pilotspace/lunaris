//! [`LlamaCppEmbedder`] — granite-embedding-311m GGUF via in-process
//! llama.cpp (llama-cpp-2 FFI, static-linked; no external server process).
//!
//! Output contract matches `lunaris-embed-native`: CLS-pooled (llama.cpp
//! `LLAMA_POOLING_TYPE_CLS`), L2-normalized, 768-d rows, `out[i]` is the
//! embedding of `inputs[i]`.
//!
//! Spike-scope design decisions (ADR
//! `docs/decisions/2026-07-10-llamacpp-inference-runtime.md`):
//!
//! - **Context per `embed_blocking` call.** `LlamaModel::new_context`
//!   borrows the model (`LlamaContext<'a>`), so storing a long-lived
//!   context next to the model would need a self-referential cell. A fresh
//!   context per call sidesteps that, costs one compute-buffer allocation
//!   (~ms — llama.cpp's buffers are FIXED size, the very property the ADR
//!   wants vs candle's shape-keyed Metal cache), and makes concurrent
//!   `embed_batch` calls safe without a model mutex (`LlamaModel` is
//!   `Sync`; each call owns its context).
//! - **Token-budget windows, not row-count windows.** Sequences are packed
//!   into one `LlamaBatch` until the next would exceed `max_batch_tokens`,
//!   then decoded together — llama.cpp packs ragged sequences into the
//!   ubatch without padding, so the §4b padding-waste ceiling has no
//!   equivalent here.
//! - Per-input truncation to `n_ctx_train` (8192 for granite-r2) mirrors
//!   the native tokenizer's `max_position_embeddings` truncation.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use lunaris_core::{Embedder, LunarisError, StorageError};

/// granite-embedding-311m output dimensionality — must agree with
/// `lunaris_embed_native::GRANITE_R2_DIM` (verified at `open`).
pub const GRANITE_R2_DIM: usize = 768;

/// Public construction options.
#[derive(Clone, Debug)]
pub struct LlamaCppEmbedderOpts {
    /// Path to the Q4_K_M (or any) granite-r2 GGUF.
    pub gguf_path: PathBuf,
    /// Layers to offload to the GPU backend. `0` = pure CPU;
    /// `u32::MAX` = everything (the useful value under `--features metal`).
    pub n_gpu_layers: u32,
    /// Forward-pass threads; `None` = llama.cpp's default.
    pub n_threads: Option<i32>,
    /// Token budget per decode window (also sizes `n_ctx`/`n_batch`/
    /// `n_ubatch`). Longer single inputs are truncated to
    /// `min(self, n_ctx_train)`.
    pub max_batch_tokens: u32,
}

impl Default for LlamaCppEmbedderOpts {
    fn default() -> Self {
        Self { gguf_path: PathBuf::new(), n_gpu_layers: 0, n_threads: None, max_batch_tokens: 4096 }
    }
}

/// Errors raised during construction / the hot path.
#[derive(Debug, thiserror::Error)]
pub enum LlamaCppEmbedderError {
    #[error(
        "granite-r2 GGUF missing at {0} — stage the Q4_K_M artifact (see \
         docs/design/quantized-inference-extractor-reranker.md §Facts) or point \
         LUNARIS_EMBEDDER_GGUF at it"
    )]
    WeightsMissing(PathBuf),

    #[error("llama.cpp: {0}")]
    Llama(String),

    #[error("model reports n_embd={0}, expected {GRANITE_R2_DIM} (wrong GGUF?)")]
    WrongDim(i32),
}

impl From<LlamaCppEmbedderError> for LunarisError {
    fn from(e: LlamaCppEmbedderError) -> Self {
        LunarisError::Storage(StorageError::Backend(format!("lunaris-llamacpp: {e}")))
    }
}

struct Inner {
    backend: LlamaBackend,
    model: LlamaModel,
    n_threads: Option<i32>,
    max_batch_tokens: u32,
}

/// llama.cpp-backed embedder. Cheap to clone — heavy state behind `Arc`.
#[derive(Clone)]
pub struct LlamaCppEmbedder {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LlamaCppEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlamaCppEmbedder")
            .field("dim", &GRANITE_R2_DIM)
            .field("max_batch_tokens", &self.inner.max_batch_tokens)
            .finish()
    }
}

impl LlamaCppEmbedder {
    /// Construct from a GGUF on disk. Loads weights synchronously; callers
    /// concerned about runtime stalls should wrap in `spawn_blocking`.
    ///
    /// `LlamaBackend::init` is once-per-process; a second `open` in the same
    /// process surfaces llama-cpp-2's `BackendAlreadyInitialized` as
    /// [`LlamaCppEmbedderError::Llama`] — one embedder per process is the
    /// spike contract.
    pub fn open(opts: LlamaCppEmbedderOpts) -> Result<Self, LlamaCppEmbedderError> {
        if !opts.gguf_path.exists() {
            return Err(LlamaCppEmbedderError::WeightsMissing(opts.gguf_path));
        }
        let backend =
            LlamaBackend::init().map_err(|e| LlamaCppEmbedderError::Llama(e.to_string()))?;
        let model_params = LlamaModelParams::default().with_n_gpu_layers(opts.n_gpu_layers);
        let model = LlamaModel::load_from_file(&backend, &opts.gguf_path, &model_params)
            .map_err(|e| LlamaCppEmbedderError::Llama(e.to_string()))?;
        let n_embd = model.n_embd();
        if n_embd as usize != GRANITE_R2_DIM {
            return Err(LlamaCppEmbedderError::WrongDim(n_embd));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                backend,
                model,
                n_threads: opts.n_threads,
                max_batch_tokens: opts.max_batch_tokens.max(16),
            }),
        })
    }

    /// Synchronous embed path — the async trait method wraps this in
    /// `spawn_blocking`.
    pub fn embed_blocking(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LlamaCppEmbedderError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let inner = &*self.inner;
        let budget = inner.max_batch_tokens.min(inner.model.n_ctx_train()) as usize;

        // Tokenize everything up front (cheap), truncating each input to the
        // window budget so a single long document can always form a window.
        let mut token_lists = Vec::with_capacity(inputs.len());
        for text in inputs {
            let mut tokens = inner
                .model
                .str_to_token(text, AddBos::Always)
                .map_err(|e| LlamaCppEmbedderError::Llama(e.to_string()))?;
            tokens.truncate(budget);
            token_lists.push(tokens);
        }

        let mut ctx_params = LlamaContextParams::default()
            .with_embeddings(true)
            .with_pooling_type(LlamaPoolingType::Cls)
            .with_n_ctx(std::num::NonZeroU32::new(budget as u32))
            .with_n_batch(budget as u32)
            .with_n_ubatch(budget as u32)
            // The CONTEXT enforces its own sequence ceiling (default 1) —
            // batch init fails with a bare "failed to initialize batch" when
            // a seq id ≥ n_seq_max shows up. Size it to the call's fan-out.
            .with_n_seq_max(inputs.len() as u32);
        if let Some(t) = inner.n_threads {
            ctx_params = ctx_params.with_n_threads(t);
        }
        let mut ctx = inner
            .model
            .new_context(&inner.backend, ctx_params)
            .map_err(|e| LlamaCppEmbedderError::Llama(e.to_string()))?;

        let mut out: Vec<Vec<f32>> = vec![Vec::new(); inputs.len()];
        let mut batch = LlamaBatch::new(budget, inputs.len() as i32);
        let mut window: Vec<usize> = Vec::new(); // input indices in the batch
        let mut used = 0usize;

        for (i, tokens) in token_lists.iter().enumerate() {
            if !window.is_empty() && used + tokens.len() > budget {
                flush_window(&mut ctx, &mut batch, &mut window, &mut out)?;
                used = 0;
            }
            // logits_all=true: pooled (CLS/MEAN) embeddings need every token
            // flagged for output — llama.cpp's batch validation rejects an
            // encoder batch with only last-token outputs ("failed to
            // initialize batch"). Mirrors upstream's embedding example.
            batch
                .add_sequence(tokens, window.len() as i32, true)
                .map_err(|e| LlamaCppEmbedderError::Llama(e.to_string()))?;
            window.push(i);
            used += tokens.len();
        }
        flush_window(&mut ctx, &mut batch, &mut window, &mut out)?;

        Ok(out)
    }
}

/// Decode the packed batch and scatter each sequence's pooled embedding back
/// to its input slot; resets the batch + window for the next fill.
fn flush_window(
    ctx: &mut llama_cpp_2::context::LlamaContext<'_>,
    batch: &mut LlamaBatch,
    window: &mut Vec<usize>,
    out: &mut [Vec<f32>],
) -> Result<(), LlamaCppEmbedderError> {
    if window.is_empty() {
        return Ok(());
    }
    // granite-r2 is encoder-only (ModernBERT): llama.cpp requires
    // `llama_encode` there — `llama_decode` returns -1 for encoder-only
    // models (surfaced by llama-cpp-2 as the misleading "n_tokens == 0").
    ctx.encode(batch).map_err(|e| LlamaCppEmbedderError::Llama(e.to_string()))?;
    for (seq, &input_idx) in window.iter().enumerate() {
        let emb = ctx
            .embeddings_seq_ith(seq as i32)
            .map_err(|e| LlamaCppEmbedderError::Llama(e.to_string()))?;
        out[input_idx] = l2_normalize(emb);
    }
    batch.clear();
    window.clear();
    Ok(())
}

fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 { v.iter().map(|x| x / norm).collect() } else { v.to_vec() }
}

#[async_trait]
impl Embedder for LlamaCppEmbedder {
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
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            me.embed_blocking(&refs).map_err(LunarisError::from)
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("lunaris-llamacpp join: {e}")))
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trait dyn-compat: `Arc<dyn Embedder>` is the public construction
    /// pattern (`Lunaris::try_with_embedder`).
    #[allow(dead_code)]
    fn _dyn_compat(e: LlamaCppEmbedder) -> std::sync::Arc<dyn Embedder> {
        std::sync::Arc::new(e)
    }

    #[test]
    fn missing_weights_fail_fast_with_actionable_error() {
        let missing = PathBuf::from(format!(
            "/tmp/lunaris-nonexistent-llamacpp-gguf-{}.gguf",
            std::process::id()
        ));
        let err = LlamaCppEmbedder::open(LlamaCppEmbedderOpts {
            gguf_path: missing.clone(),
            ..Default::default()
        })
        .expect_err("missing GGUF must error before touching llama.cpp");
        match err {
            LlamaCppEmbedderError::WeightsMissing(p) => assert_eq!(p, missing),
            other => panic!("expected WeightsMissing, got {other:?}"),
        }
    }

    #[test]
    fn l2_normalize_unit_norm_and_zero_safety() {
        let n = l2_normalize(&[3.0, 4.0]);
        assert!((n.iter().map(|x| x * x).sum::<f32>().sqrt() - 1.0).abs() < 1e-6);
        assert_eq!(l2_normalize(&[0.0, 0.0]), vec![0.0, 0.0]);
    }
}
