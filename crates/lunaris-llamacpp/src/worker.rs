//! [`EncodeWorker`] — Phase A2 context pool. A dedicated OS thread owns the
//! model handle plus ONE long-lived llama.cpp context and serves encode
//! requests over a channel; callers get raw CLS-pooled per-sequence
//! embeddings back and do their own post-processing (embedder:
//! L2-normalize; reranker: classification head).
//!
//! Why a worker thread and not a pooled field:
//! - `LlamaContext<'a>` borrows `LlamaModel`, so storing both in one struct
//!   is self-referential; inside the worker's stack frame the borrow is
//!   ordinary and safe.
//! - The context is created ONCE — repeat calls hit a warm context. The
//!   spike's context-per-call setup was what capped Metal at 1,266 tok/s
//!   vs the 13,650 tok/s warm ceiling (ADR §spike-results).
//! - Fixed buffers, no leak class, and everything is freed on drop: the
//!   handle's `Sender` drops → `recv` errors → the thread exits, dropping
//!   context then model Arc. No `Box::leak`.
//!
//! Concurrent callers serialize through the channel — one context is the
//! deliberate footprint contract (same design as llama.cpp's own server).

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::token::LlamaToken;

use crate::backend::shared_backend;

/// Sequence-count ceiling per encode window — the context's `n_seq_max`.
/// Windows flush at whichever ceiling hits first: token budget or this.
const MAX_SEQS_PER_WINDOW: usize = 64;

struct Job {
    token_lists: Vec<Vec<LlamaToken>>,
    reply: mpsc::Sender<Result<Vec<Vec<f32>>, String>>,
}

pub(crate) struct EncodeWorker {
    /// `Option` so `Drop` can hang up the channel before joining.
    tx: Option<mpsc::Sender<Job>>,
    handle: Option<std::thread::JoinHandle<()>>,
    created: Arc<AtomicUsize>,
}

impl Drop for EncodeWorker {
    fn drop(&mut self) {
        // Hang up → worker's recv errors → it tears down context + model
        // Arc; JOIN so process exit never races a thread still inside
        // llama.cpp (observed as a SIGSEGV in test teardown when the
        // thread was detached).
        drop(self.tx.take());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl EncodeWorker {
    /// Spawn the worker and BLOCK until its context exists — construction
    /// errors (bad GGUF, GPU init failure) surface here, preserving the
    /// fail-fast `open()` contract.
    pub(crate) fn spawn(
        model: Arc<LlamaModel>,
        budget: usize,
        n_threads: Option<i32>,
        thread_name: &str,
    ) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let created = Arc::new(AtomicUsize::new(0));
        let created_in_thread = Arc::clone(&created);

        let handle = std::thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                let backend = match shared_backend() {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let mut params = LlamaContextParams::default()
                    .with_embeddings(true)
                    .with_pooling_type(LlamaPoolingType::Cls)
                    .with_n_ctx(NonZeroU32::new(budget as u32))
                    .with_n_batch(budget as u32)
                    .with_n_ubatch(budget as u32)
                    .with_n_seq_max(MAX_SEQS_PER_WINDOW as u32);
                if let Some(t) = n_threads {
                    params = params.with_n_threads(t);
                }
                let mut ctx = match model.new_context(backend, params) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                created_in_thread.fetch_add(1, Ordering::SeqCst);
                let _ = ready_tx.send(Ok(()));

                let mut batch = LlamaBatch::new(budget, MAX_SEQS_PER_WINDOW as i32);
                while let Ok(job) = rx.recv() {
                    let result = encode_all(&mut ctx, &mut batch, &job.token_lists, budget);
                    let _ = job.reply.send(result);
                }
                // Sender side hung up: handle dropped. Context, batch, and
                // the model Arc clone drop here — full teardown.
            })
            .map_err(|e| format!("spawn llama.cpp worker: {e}"))?;

        ready_rx
            .recv()
            .map_err(|_| "llama.cpp worker died before creating its context".to_string())??;
        Ok(Self { tx: Some(tx), handle: Some(handle), created })
    }

    /// Encode token lists → raw CLS-pooled embeddings, in input order.
    pub(crate) fn encode(
        &self,
        token_lists: Vec<Vec<LlamaToken>>,
    ) -> Result<Vec<Vec<f32>>, String> {
        if token_lists.is_empty() {
            return Ok(Vec::new());
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .as_ref()
            .expect("tx only vacated in Drop")
            .send(Job { token_lists, reply: reply_tx })
            .map_err(|_| "llama.cpp worker is gone".to_string())?;
        reply_rx.recv().map_err(|_| "llama.cpp worker died mid-encode".to_string())?
    }

    /// How many llama.cpp contexts this worker has ever created — exactly 1
    /// for the lifetime of the handle. Exposed for the A2 context-reuse
    /// regression test (context-per-call would count one per encode).
    pub(crate) fn contexts_created(&self) -> usize {
        self.created.load(Ordering::SeqCst)
    }
}

/// Pack sequences into token-budget + seq-count windows against the ONE
/// warm context; scatter each sequence's pooled embedding to its input slot.
fn encode_all(
    ctx: &mut LlamaContext<'_>,
    batch: &mut LlamaBatch,
    token_lists: &[Vec<LlamaToken>],
    budget: usize,
) -> Result<Vec<Vec<f32>>, String> {
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); token_lists.len()];
    let mut window: Vec<usize> = Vec::new();
    let mut used = 0usize;

    for (i, tokens) in token_lists.iter().enumerate() {
        let over_tokens = used + tokens.len() > budget;
        let over_seqs = window.len() >= MAX_SEQS_PER_WINDOW;
        if !window.is_empty() && (over_tokens || over_seqs) {
            flush(ctx, batch, &mut window, &mut out)?;
            used = 0;
        }
        // logits_all=true: pooled embeddings need every token flagged for
        // output (llama.cpp rejects encoder batches with last-token-only
        // outputs — "failed to initialize batch").
        batch.add_sequence(tokens, window.len() as i32, true).map_err(|e| e.to_string())?;
        window.push(i);
        used += tokens.len();
    }
    flush(ctx, batch, &mut window, &mut out)?;
    Ok(out)
}

fn flush(
    ctx: &mut LlamaContext<'_>,
    batch: &mut LlamaBatch,
    window: &mut Vec<usize>,
    out: &mut [Vec<f32>],
) -> Result<(), String> {
    if window.is_empty() {
        return Ok(());
    }
    // Encoder-only models need llama_encode (llama_decode returns a bare -1
    // mislabeled "n_tokens == 0").
    ctx.encode(batch).map_err(|e| e.to_string())?;
    for (seq, &input_idx) in window.iter().enumerate() {
        let emb = ctx.embeddings_seq_ith(seq as i32).map_err(|e| e.to_string())?;
        out[input_idx] = emb.to_vec();
    }
    batch.clear();
    window.clear();
    Ok(())
}
