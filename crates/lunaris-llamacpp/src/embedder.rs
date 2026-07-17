//! [`LlamaCppEmbedder`] — granite-embedding-311m GGUF via in-process
//! llama.cpp (llama-cpp-2 FFI, static-linked; no external server process).
//!
//! Output contract (carried over from the retired candle embedder,
//! llama.cpp-only cutover): CLS-pooled (llama.cpp
//! `LLAMA_POOLING_TYPE_CLS`), L2-normalized, 768-d rows, `out[i]` is the
//! embedding of `inputs[i]`.
//!
//! Design (ADR `docs/decisions/2026-07-10-llamacpp-inference-runtime.md`
//! + cutover Phase A2):
//!
//! - **One warm context on a worker thread** (`crate::worker::EncodeWorker`)
//!   — the spike's context-per-call design paid llama.cpp's context setup on
//!   every batch, which capped Metal at ~1/10 of its warm ceiling. The
//!   worker owns model + context (no self-referential struct), keeps
//!   llama.cpp's FIXED compute buffers (the anti-leak property the ADR
//!   wants vs candle's shape-keyed Metal cache), and frees everything on
//!   drop.
//! - **Token-budget windows, not row-count windows.** Sequences are packed
//!   into one `LlamaBatch` until the next would exceed the budget (or the
//!   context's `n_seq_max`), then encoded together — llama.cpp packs ragged
//!   sequences into the ubatch without padding, so the §4b padding-waste
//!   ceiling has no equivalent here.
//! - Per-input truncation to `n_ctx_train` (8192 for granite-r2) mirrors
//!   the native tokenizer's `max_position_embeddings` truncation.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::token::LlamaToken;

use crate::teardown::EngineCell;
use crate::worker::{EncodeWorker, MAX_SEQS_PER_WINDOW, Priority};
use lunaris_core::{Embedder, LunarisError, StorageError};

/// granite-embedding-311m output dimensionality — must agree with
/// granite-r2's published 768-d output dim (verified at `open`).
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

    #[error(
        "embedder engine closed — process inference teardown already ran \
         (shutdown_all_inference, normally the host's atexit hook)"
    )]
    Closed,
}

impl From<LlamaCppEmbedderError> for LunarisError {
    fn from(e: LlamaCppEmbedderError) -> Self {
        LunarisError::Storage(StorageError::Backend(format!("lunaris-llamacpp: {e}")))
    }
}

struct Inner {
    model: Arc<LlamaModel>,
    worker: EncodeWorker,
    /// Effective per-window token budget — fixed at `open` (also sizes the
    /// worker context), callers truncate against it.
    budget: usize,
}

/// llama.cpp-backed embedder. Cheap to clone — heavy state behind `Arc`,
/// parked in a takeable `EngineCell` so process-wide inference teardown
/// (`crate::shutdown_all_inference`, exit-time Metal safety) can free the
/// model + worker even when a host runtime leaks this handle.
#[derive(Clone)]
pub struct LlamaCppEmbedder {
    cell: Arc<EngineCell<Inner>>,
}

impl std::fmt::Debug for LlamaCppEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlamaCppEmbedder")
            .field("dim", &GRANITE_R2_DIM)
            .field("budget", &self.cell.get().map(|i| i.budget))
            .finish()
    }
}

impl LlamaCppEmbedder {
    /// Construct from a GGUF on disk. Loads weights + spawns the encode
    /// worker (model + ONE warm context on a dedicated thread — Phase A2)
    /// synchronously; callers concerned about runtime stalls should wrap in
    /// `spawn_blocking`.
    ///
    /// Uses the process-shared `LlamaBackend` (`crate::backend`), so any
    /// number of models — embedder + reranker + later extractor — coexist
    /// in one process.
    pub fn open(opts: LlamaCppEmbedderOpts) -> Result<Self, LlamaCppEmbedderError> {
        if !opts.gguf_path.exists() {
            return Err(LlamaCppEmbedderError::WeightsMissing(opts.gguf_path));
        }
        let backend = crate::backend::shared_backend().map_err(LlamaCppEmbedderError::Llama)?;
        let model_params = LlamaModelParams::default().with_n_gpu_layers(opts.n_gpu_layers);
        let model = LlamaModel::load_from_file(backend, &opts.gguf_path, &model_params)
            .map_err(|e| LlamaCppEmbedderError::Llama(e.to_string()))?;
        let n_embd = model.n_embd();
        if n_embd as usize != GRANITE_R2_DIM {
            return Err(LlamaCppEmbedderError::WrongDim(n_embd));
        }
        let model = Arc::new(model);
        let budget = opts.max_batch_tokens.max(16).min(model.n_ctx_train()) as usize;
        let worker = EncodeWorker::spawn(
            Arc::clone(&model),
            budget,
            opts.n_threads,
            "lunaris-llamacpp-embed",
        )
        .map_err(LlamaCppEmbedderError::Llama)?;
        Ok(Self { cell: EngineCell::new(Arc::new(Inner { model, worker, budget })) })
    }

    /// Live inner, or [`LlamaCppEmbedderError::Closed`] once process
    /// inference teardown has run.
    fn inner(&self) -> Result<Arc<Inner>, LlamaCppEmbedderError> {
        self.cell.get().ok_or(LlamaCppEmbedderError::Closed)
    }

    /// How many llama.cpp contexts this handle has ever created — exactly 1
    /// (the A2 warm-context contract; see `tests/context_reuse.rs`).
    /// 0 after process inference teardown.
    pub fn contexts_created(&self) -> usize {
        self.cell.get().map(|i| i.worker.contexts_created()).unwrap_or(0)
    }

    /// Synchronous embed path — the async trait method wraps this in
    /// `spawn_blocking`. Tokenizes caller-side (the model handle is shared
    /// with the worker), encodes on the warm worker context, L2-normalizes.
    ///
    /// Interactive lane (recall queries): one encode job for the whole batch.
    pub fn embed_blocking(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LlamaCppEmbedderError> {
        self.embed_blocking_prio(inputs, Priority::Interactive)
    }

    /// Priority-aware sync embed. `Interactive` submits the whole batch as one
    /// job; `Background` splits it into window-sized jobs (≤ `budget` tokens and
    /// ≤ `MAX_SEQS_PER_WINDOW` sequences each) so an interactive query can slip
    /// in after one background window instead of the whole batch. Embeddings
    /// are byte-identical across lanes — priority is scheduling only.
    fn embed_blocking_prio(
        &self,
        inputs: &[&str],
        priority: Priority,
    ) -> Result<Vec<Vec<f32>>, LlamaCppEmbedderError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let inner = self.inner()?;

        // Tokenize everything up front (cheap), truncating each input to the
        // window budget so a single long document can always form a window.
        let mut token_lists = Vec::with_capacity(inputs.len());
        for text in inputs {
            let mut tokens = inner
                .model
                .str_to_token(text, AddBos::Always)
                .map_err(|e| LlamaCppEmbedderError::Llama(e.to_string()))?;
            tokens.truncate(inner.budget);
            token_lists.push(tokens);
        }

        match priority {
            Priority::Interactive => {
                let raw = inner
                    .worker
                    .encode(token_lists, Priority::Interactive)
                    .map_err(LlamaCppEmbedderError::Llama)?;
                Ok(raw.iter().map(|row| l2_normalize(row)).collect())
            }
            Priority::Background => {
                // Pack sequences into window-sized jobs so the worker yields to
                // the Interactive lane between windows. Order is preserved: we
                // split sequentially and concatenate results in order.
                let mut out = Vec::with_capacity(token_lists.len());
                let mut window: Vec<Vec<LlamaToken>> = Vec::new();
                let mut used = 0usize;
                for tokens in token_lists {
                    let over_tokens = used + tokens.len() > inner.budget;
                    let over_seqs = window.len() >= MAX_SEQS_PER_WINDOW;
                    if !window.is_empty() && (over_tokens || over_seqs) {
                        let raw = inner
                            .worker
                            .encode(std::mem::take(&mut window), Priority::Background)
                            .map_err(LlamaCppEmbedderError::Llama)?;
                        out.extend(raw.iter().map(|row| l2_normalize(row)));
                        used = 0;
                    }
                    used += tokens.len();
                    window.push(tokens);
                }
                if !window.is_empty() {
                    let raw = inner
                        .worker
                        .encode(window, Priority::Background)
                        .map_err(LlamaCppEmbedderError::Llama)?;
                    out.extend(raw.iter().map(|row| l2_normalize(row)));
                }
                Ok(out)
            }
        }
    }

    /// Async wrapper shared by both trait lanes: hop onto a blocking thread and
    /// run the priority-aware sync path. `Interactive` for recall queries,
    /// `Background` for ingest promotion.
    async fn embed_batch_prio(
        &self,
        inputs: &[&str],
        priority: Priority,
    ) -> Result<Vec<Vec<f32>>, LunarisError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let owned: Vec<String> = inputs.iter().map(|s| (*s).to_string()).collect();
        let me = self.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>, LunarisError> {
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            me.embed_blocking_prio(&refs, priority).map_err(LunarisError::from)
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("lunaris-llamacpp join: {e}")))
        })?
    }
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
        self.embed_batch_prio(inputs, Priority::Interactive).await
    }

    /// Background lane: ingest promotion embeds via this so it never head-of-
    /// line-blocks an interactive recall query on the shared worker context.
    async fn embed_batch_lowpri(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        self.embed_batch_prio(inputs, Priority::Background).await
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
