//! [`LlamaCppReranker`] — bge-reranker-v2-m3 GGUF cross-encoder via
//! in-process llama.cpp. Phase A1 of the runtime cutover (unknown #1).
//!
//! Output contract matches `lunaris-rerank-native`: sigmoid scores in
//! `[0, 1]`, one per candidate; the `lunaris_rerank::Reranker` impl returns
//! exactly `docs.len()` candidates sorted score-desc.
//!
//! ## Why CLS pooling + a Rust-side head (not `LLAMA_POOLING_TYPE_RANK`)
//!
//! The pinned `llama-cpp-2 =0.1.151` sizes `embeddings_seq_ith` slices by
//! `n_embd` unconditionally, but Rank pooling stores exactly one float per
//! sequence — the safe accessor would read out of bounds. CLS pooling
//! stores a correctly `n_embd`-sized buffer, so the encoder runs under CLS
//! and the XLM-R classification head (`dense → tanh → out_proj → sigmoid`,
//! the same math as `lunaris-rerank-native::quantized_xlmr`) is applied in
//! Rust from weights read directly out of the GGUF (`gguf_head`). The head
//! is a single 1024×1024 GEMV per candidate — microseconds against a
//! multi-hundred-ms encoder pass.
//!
//! ## Pair encoding
//!
//! Mirrors llama.cpp server's `format_rerank`:
//! `[BOS] query [EOS] [SEP] doc [EOS]` — for XLM-R (`bos=<s>`,
//! `eos=sep=</s>`) this reproduces the HF cross-encoder pair encoding
//! `<s> query </s></s> doc </s>` exactly.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::token::LlamaToken;
use lunaris_core::{LunarisError, StorageError};
use lunaris_rerank::{RerankCandidate, Reranker};

use crate::backend::shared_backend;
use crate::gguf_head::{ClsHead, GgufHeadError};

/// bge-reranker-v2-m3 hidden size — the encoder's `n_embd` and the
/// classification head's input width (verified at `open`).
pub const BGE_RERANKER_DIM: usize = 1024;

/// Public construction options — mirrors [`crate::LlamaCppEmbedderOpts`].
#[derive(Clone, Debug)]
pub struct LlamaCppRerankerOpts {
    /// Path to the Q5_K_M (or any) bge-reranker-v2-m3 GGUF.
    pub gguf_path: PathBuf,
    /// Layers to offload to the GPU backend. `0` = pure CPU;
    /// `u32::MAX` = everything (the useful value under `--features metal`).
    pub n_gpu_layers: u32,
    /// Forward-pass threads; `None` = llama.cpp's default.
    pub n_threads: Option<i32>,
    /// Token budget per encode window (also sizes `n_ctx`/`n_batch`/
    /// `n_ubatch`). Pairs longer than `min(self, n_ctx_train)` get their
    /// DOC tail truncated (the query is preserved whole where possible).
    pub max_batch_tokens: u32,
}

impl Default for LlamaCppRerankerOpts {
    fn default() -> Self {
        Self { gguf_path: PathBuf::new(), n_gpu_layers: 0, n_threads: None, max_batch_tokens: 8192 }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LlamaCppRerankerError {
    #[error("bge reranker GGUF not found at {0} — stage the model or set LUNARIS_RERANKER_GGUF")]
    WeightsMissing(PathBuf),
    #[error("model n_embd {0} != bge-reranker-v2-m3 dim 1024 — wrong GGUF?")]
    WrongDim(i32),
    #[error("classification head: {0}")]
    Head(#[from] GgufHeadError),
    #[error("llama.cpp: {0}")]
    Llama(String),
}

impl From<LlamaCppRerankerError> for LunarisError {
    fn from(e: LlamaCppRerankerError) -> Self {
        LunarisError::Storage(StorageError::Backend(format!("lunaris-llamacpp rerank: {e}")))
    }
}

struct Inner {
    model: LlamaModel,
    head: ClsHead,
    n_threads: Option<i32>,
    max_batch_tokens: u32,
}

/// llama.cpp-backed cross-encoder reranker. Cheap to clone — heavy state
/// behind `Arc`; context-per-call, same rationale as the embedder.
#[derive(Clone)]
pub struct LlamaCppReranker {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LlamaCppReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlamaCppReranker")
            .field("dim", &BGE_RERANKER_DIM)
            .field("max_batch_tokens", &self.inner.max_batch_tokens)
            .finish()
    }
}

impl LlamaCppReranker {
    /// Construct from a GGUF on disk. Loads weights + the classification
    /// head synchronously; wrap in `spawn_blocking` if runtime stalls
    /// matter. Uses the process-shared `LlamaBackend`, so it coexists with
    /// [`crate::LlamaCppEmbedder`] in one process.
    pub fn open(opts: LlamaCppRerankerOpts) -> Result<Self, LlamaCppRerankerError> {
        if !opts.gguf_path.exists() {
            return Err(LlamaCppRerankerError::WeightsMissing(opts.gguf_path));
        }
        let backend = shared_backend().map_err(LlamaCppRerankerError::Llama)?;
        let model_params = LlamaModelParams::default().with_n_gpu_layers(opts.n_gpu_layers);
        let model = LlamaModel::load_from_file(backend, &opts.gguf_path, &model_params)
            .map_err(|e| LlamaCppRerankerError::Llama(e.to_string()))?;
        let n_embd = model.n_embd();
        if n_embd as usize != BGE_RERANKER_DIM {
            return Err(LlamaCppRerankerError::WrongDim(n_embd));
        }
        let head = crate::gguf_head::read_cls_head(&opts.gguf_path)?;
        if head.hidden != BGE_RERANKER_DIM {
            return Err(LlamaCppRerankerError::Head(GgufHeadError::Shape {
                name: "cls.bias".into(),
                dims: vec![head.hidden as u64],
            }));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                model,
                head,
                n_threads: opts.n_threads,
                max_batch_tokens: opts.max_batch_tokens.max(16),
            }),
        })
    }

    /// Synchronous scoring path — sigmoid score per doc, in INPUT order.
    /// The async trait method wraps this in `spawn_blocking`.
    pub fn score_blocking(
        &self,
        query: &str,
        docs: &[&str],
    ) -> Result<Vec<f32>, LlamaCppRerankerError> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }
        let inner = &*self.inner;
        let budget = inner.max_batch_tokens.min(inner.model.n_ctx_train()) as usize;
        let llama = |e: std::fmt::Arguments<'_>| LlamaCppRerankerError::Llama(e.to_string());

        let bos = inner.model.token_bos();
        let eos = inner.model.token_eos();
        let sep = inner.model.token_sep();

        let q_toks = inner
            .model
            .str_to_token(query, AddBos::Never)
            .map_err(|e| llama(format_args!("{e}")))?;

        // Build [BOS] q [EOS] [SEP] d [EOS] per pair, truncating the doc
        // tail to the window budget (and the query itself only when the
        // query alone would overflow).
        let overhead = 4usize; // bos + eos + sep + eos
        let q_keep = q_toks.len().min(budget.saturating_sub(overhead).max(1));
        let mut token_lists: Vec<Vec<LlamaToken>> = Vec::with_capacity(docs.len());
        for doc in docs {
            let d_toks = inner
                .model
                .str_to_token(doc, AddBos::Never)
                .map_err(|e| llama(format_args!("{e}")))?;
            let d_keep = d_toks.len().min(budget.saturating_sub(overhead + q_keep).max(1));
            let mut seq = Vec::with_capacity(overhead + q_keep + d_keep);
            seq.push(bos);
            seq.extend_from_slice(&q_toks[..q_keep]);
            seq.push(eos);
            seq.push(sep);
            seq.extend_from_slice(&d_toks[..d_keep]);
            seq.push(eos);
            token_lists.push(seq);
        }

        let mut ctx_params = LlamaContextParams::default()
            .with_embeddings(true)
            // CLS, not Rank — see the module doc (Rank + the pinned
            // binding's n_embd-sized slice = OOB read).
            .with_pooling_type(LlamaPoolingType::Cls)
            .with_n_ctx(std::num::NonZeroU32::new(budget as u32))
            .with_n_batch(budget as u32)
            .with_n_ubatch(budget as u32)
            .with_n_seq_max(docs.len() as u32);
        if let Some(t) = inner.n_threads {
            ctx_params = ctx_params.with_n_threads(t);
        }
        let backend = shared_backend().map_err(LlamaCppRerankerError::Llama)?;
        let mut ctx =
            inner.model.new_context(backend, ctx_params).map_err(|e| llama(format_args!("{e}")))?;

        let mut out = vec![f32::NAN; docs.len()];
        let mut batch = LlamaBatch::new(budget, docs.len() as i32);
        let mut window: Vec<usize> = Vec::new();
        let mut used = 0usize;

        for (i, tokens) in token_lists.iter().enumerate() {
            if !window.is_empty() && used + tokens.len() > budget {
                self.flush_window(&mut ctx, &mut batch, &mut window, &mut out)?;
                used = 0;
            }
            batch
                .add_sequence(tokens, window.len() as i32, true)
                .map_err(|e| llama(format_args!("{e}")))?;
            window.push(i);
            used += tokens.len();
        }
        self.flush_window(&mut ctx, &mut batch, &mut window, &mut out)?;

        Ok(out)
    }

    /// Encode the packed window, CLS-pool each sequence, apply the
    /// classification head, scatter sigmoid scores to input slots.
    fn flush_window(
        &self,
        ctx: &mut llama_cpp_2::context::LlamaContext<'_>,
        batch: &mut LlamaBatch,
        window: &mut Vec<usize>,
        out: &mut [f32],
    ) -> Result<(), LlamaCppRerankerError> {
        if window.is_empty() {
            return Ok(());
        }
        ctx.encode(batch).map_err(|e| LlamaCppRerankerError::Llama(e.to_string()))?;
        for (seq, &doc_idx) in window.iter().enumerate() {
            let cls = ctx
                .embeddings_seq_ith(seq as i32)
                .map_err(|e| LlamaCppRerankerError::Llama(e.to_string()))?;
            out[doc_idx] = head_score(&self.inner.head, cls);
        }
        batch.clear();
        window.clear();
        Ok(())
    }
}

/// XLM-R classification head: `sigmoid(out_w · tanh(W·cls + b) + out_b)` —
/// HF `Linear` row-major convention (`dense_w` row `j` = output neuron `j`).
/// Same math as `lunaris-rerank-native::quantized_xlmr`'s classifier path.
fn head_score(h: &ClsHead, cls: &[f32]) -> f32 {
    let n = h.hidden;
    let mut logit = h.out_b;
    for j in 0..n {
        let row = &h.dense_w[j * n..(j + 1) * n];
        let mut acc = h.dense_b[j];
        for (x, w) in cls.iter().zip(row) {
            acc += x * w;
        }
        logit += acc.tanh() * h.out_w[j];
    }
    1.0 / (1.0 + (-logit).exp())
}

#[async_trait]
impl Reranker for LlamaCppReranker {
    async fn rerank(
        &self,
        query: &str,
        docs: Vec<RerankCandidate>,
    ) -> Result<Vec<RerankCandidate>, LunarisError> {
        if docs.is_empty() {
            return Ok(docs);
        }
        let me = self.clone();
        let owned_query = query.to_string();
        tokio::task::spawn_blocking(move || -> Result<Vec<RerankCandidate>, LunarisError> {
            let texts: Vec<&str> = docs.iter().map(|d| d.text.as_str()).collect();
            let scores = me.score_blocking(&owned_query, &texts).map_err(LunarisError::from)?;
            let mut scored: Vec<RerankCandidate> = docs
                .into_iter()
                .zip(scores)
                .map(|(mut c, s)| {
                    c.score = s;
                    c
                })
                .collect();
            scored
                .sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
            Ok(scored)
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("lunaris-llamacpp join: {e}")))
        })?
    }

    fn applies(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trait dyn-compat mirrors the operator wiring contract.
    #[allow(dead_code)]
    fn _dyn_compat(r: LlamaCppReranker) -> std::sync::Arc<dyn Reranker> {
        std::sync::Arc::new(r)
    }

    #[test]
    fn missing_weights_fail_fast_with_actionable_error() {
        let missing = PathBuf::from(format!(
            "/tmp/lunaris-nonexistent-llamacpp-rerank-{}.gguf",
            std::process::id()
        ));
        let err = LlamaCppReranker::open(LlamaCppRerankerOpts {
            gguf_path: missing.clone(),
            ..Default::default()
        })
        .expect_err("missing GGUF must error before touching llama.cpp");
        match err {
            LlamaCppRerankerError::WeightsMissing(p) => assert_eq!(p, missing),
            other => panic!("expected WeightsMissing, got {other:?}"),
        }
    }

    /// Pure head math against a hand-computed reference: hidden=2,
    /// W = [[1, 0], [0, -1]] (row-major), b = [0, 0.5], out_w = [1, 2],
    /// out_b = 0.25, cls = [0.3, 0.4] →
    /// h = [tanh(0.3), tanh(0.5 − 0.4)], logit = 0.25 + tanh(0.3) + 2·tanh(0.1).
    #[test]
    fn head_score_matches_hand_reference() {
        let h = ClsHead {
            hidden: 2,
            dense_w: vec![1.0, 0.0, 0.0, -1.0],
            dense_b: vec![0.0, 0.5],
            out_w: vec![1.0, 2.0],
            out_b: 0.25,
        };
        let expected_logit = 0.25 + 0.3f32.tanh() + 2.0 * 0.1f32.tanh();
        let expected = 1.0 / (1.0 + (-expected_logit).exp());
        let got = head_score(&h, &[0.3, 0.4]);
        assert!((got - expected).abs() < 1e-6, "got {got}, expected {expected}");
    }
}
