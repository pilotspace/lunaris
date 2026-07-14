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

use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::token::LlamaToken;

use crate::backend::shared_backend;

/// Sequence-count ceiling per encode window — the context's `n_seq_max`.
/// Windows flush at whichever ceiling hits first: token budget or this.
/// `pub(crate)` so the Background window-splitter (embedder) caps each
/// submitted job to one real encode window.
pub(crate) const MAX_SEQS_PER_WINDOW: usize = 64;

/// Which lane a job rides. `Interactive` (recall queries) always drains before
/// `Background` (ingest promotion) so an interactive embed never head-of-line-
/// blocks behind a large background batch. Priority is SCHEDULING only — it
/// never changes the embedding a job produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Priority {
    Interactive,
    Background,
}

struct Lanes<T> {
    high: VecDeque<T>,
    low: VecDeque<T>,
    /// Set once the worker is shutting down; `push` becomes a no-op (the item
    /// is dropped so its caller's reply channel errors instead of hanging).
    closed: bool,
}

/// Two-lane intake in front of the single encode context. std-only (no
/// crossbeam — it's unvendored here and carries an active advisory): a
/// `Mutex` over the two deques plus a `Condvar` for the blocking worker pop.
/// The worker thread is plain sync code, so there is no lock-across-await.
pub(crate) struct PriorityIntake<T> {
    lanes: Mutex<Lanes<T>>,
    cv: Condvar,
}

impl<T> PriorityIntake<T> {
    pub(crate) fn new() -> Self {
        Self {
            lanes: Mutex::new(Lanes { high: VecDeque::new(), low: VecDeque::new(), closed: false }),
            cv: Condvar::new(),
        }
    }

    /// Enqueue `item` on its lane and wake the worker. Fire-and-forget: if the
    /// intake is already closed the item is dropped (a closed worker would
    /// never consume it — dropping lets the caller's reply channel error).
    pub(crate) fn push(&self, priority: Priority, item: T) {
        let mut lanes = self.lanes.lock().expect("intake mutex poisoned");
        if lanes.closed {
            return;
        }
        match priority {
            Priority::Interactive => lanes.high.push_back(item),
            Priority::Background => lanes.low.push_back(item),
        }
        drop(lanes);
        self.cv.notify_one();
    }

    /// Non-blocking pop: Interactive lane first, then Background, else `None`.
    pub(crate) fn try_pop(&self) -> Option<T> {
        let mut lanes = self.lanes.lock().expect("intake mutex poisoned");
        if let Some(item) = lanes.high.pop_front() {
            return Some(item);
        }
        lanes.low.pop_front()
    }

    /// Block until a job is available (Interactive first) or the intake is
    /// closed. Returns `None` only when closed AND both lanes are drained — the
    /// worker's exit signal.
    fn pop_blocking(&self) -> Option<T> {
        let mut lanes = self.lanes.lock().expect("intake mutex poisoned");
        loop {
            if let Some(item) = lanes.high.pop_front() {
                return Some(item);
            }
            if let Some(item) = lanes.low.pop_front() {
                return Some(item);
            }
            if lanes.closed {
                return None;
            }
            lanes = self.cv.wait(lanes).expect("intake mutex poisoned");
        }
    }

    /// Signal no more work will be consumed; wake the worker so it can drain
    /// and exit. Idempotent.
    fn close(&self) {
        let mut lanes = self.lanes.lock().expect("intake mutex poisoned");
        lanes.closed = true;
        drop(lanes);
        self.cv.notify_all();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.lanes.lock().expect("intake mutex poisoned").closed
    }
}

struct Job {
    token_lists: Vec<Vec<LlamaToken>>,
    reply: mpsc::Sender<Result<Vec<Vec<f32>>, String>>,
}

/// Closes and drains the intake when the worker thread exits for ANY reason
/// (normal return or panic unwind), so a job still queued has its reply channel
/// dropped and its `encode()` caller gets an `Err` rather than blocking forever.
struct DrainOnExit(Arc<PriorityIntake<Job>>);

impl Drop for DrainOnExit {
    fn drop(&mut self) {
        self.0.close();
        while self.0.try_pop().is_some() {}
    }
}

pub(crate) struct EncodeWorker {
    intake: Arc<PriorityIntake<Job>>,
    handle: Option<std::thread::JoinHandle<()>>,
    created: Arc<AtomicUsize>,
}

impl Drop for EncodeWorker {
    fn drop(&mut self) {
        // Close the intake → worker's `pop_blocking` returns None → it tears
        // down context + model Arc; JOIN so process exit never races a thread
        // still inside llama.cpp (observed as a SIGSEGV in test teardown when
        // the thread was detached).
        self.intake.close();
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
        let intake = Arc::new(PriorityIntake::<Job>::new());
        let intake_in_thread = Arc::clone(&intake);
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let created = Arc::new(AtomicUsize::new(0));
        let created_in_thread = Arc::clone(&created);

        let handle = std::thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                // On ANY exit (normal or panic), close + drain the intake so
                // queued jobs' reply senders drop and their callers get an Err
                // rather than hanging on `reply_rx.recv()`.
                let _drain = DrainOnExit(Arc::clone(&intake_in_thread));

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
                // Interactive lane drains before Background between jobs; a
                // large background workload is pre-split into window-sized jobs
                // by the caller, so a query slips in after one background
                // window rather than the whole batch.
                while let Some(job) = intake_in_thread.pop_blocking() {
                    let result = encode_all(&mut ctx, &mut batch, &job.token_lists, budget);
                    let _ = job.reply.send(result);
                }
                // Intake closed + drained: context, batch, and the model Arc
                // clone drop here — full teardown.
            })
            .map_err(|e| format!("spawn llama.cpp worker: {e}"))?;

        ready_rx
            .recv()
            .map_err(|_| "llama.cpp worker died before creating its context".to_string())??;
        Ok(Self { intake, handle: Some(handle), created })
    }

    /// Encode token lists → raw CLS-pooled embeddings, in input order, on the
    /// given priority lane.
    pub(crate) fn encode(
        &self,
        token_lists: Vec<Vec<LlamaToken>>,
        priority: Priority,
    ) -> Result<Vec<Vec<f32>>, String> {
        if token_lists.is_empty() {
            return Ok(Vec::new());
        }
        if self.intake.is_closed() {
            return Err("llama.cpp worker is gone".to_string());
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.intake.push(priority, Job { token_lists, reply: reply_tx });
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

#[cfg(test)]
mod intake_tests {
    // Scheduling logic is model-free: `PriorityIntake` orders jobs without any
    // llama.cpp context, so these tests pin the interleave contract with no GGUF.
    use super::{Priority, PriorityIntake};

    #[test]
    fn drains_high_before_low() {
        // scenario: priority ordering under mixed submission — all Interactive
        // jobs come out before any Background job, regardless of submit order.
        let q: PriorityIntake<&'static str> = PriorityIntake::new();
        q.push(Priority::Background, "low1");
        q.push(Priority::Background, "low2");
        q.push(Priority::Interactive, "high1");
        q.push(Priority::Background, "low3");
        let mut got = Vec::new();
        while let Some(x) = q.try_pop() {
            got.push(x);
        }
        assert_eq!(got, vec!["high1", "low1", "low2", "low3"]);
    }

    #[test]
    fn fifo_within_each_lane() {
        // No starvation reorder inside a lane: submit order is preserved.
        let q: PriorityIntake<&'static str> = PriorityIntake::new();
        q.push(Priority::Interactive, "a");
        q.push(Priority::Interactive, "b");
        q.push(Priority::Background, "x");
        q.push(Priority::Background, "y");
        assert_eq!(q.try_pop(), Some("a"));
        assert_eq!(q.try_pop(), Some("b"));
        assert_eq!(q.try_pop(), Some("x"));
        assert_eq!(q.try_pop(), Some("y"));
        assert_eq!(q.try_pop(), None);
    }
}
