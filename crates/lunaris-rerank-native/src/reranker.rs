//! `NativeReranker` — `lunaris_rerank::Reranker` impl backed by candle's
//! XLM-RoBERTa cross-encoder for `BAAI/bge-reranker-v2-m3`.
//!
//! See [`crate`] for the architectural rationale. This module:
//! - holds the `Arc<Inner>` cheap-clone handle,
//! - constructs the model from safetensors + `tokenizer.json` + `config.json`,
//! - implements `Reranker::rerank` via `tokio::task::spawn_blocking` — no
//!   lock held across `.await`,
//! - sorts the returned `RerankCandidate`s by score descending.
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

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::{DType, Device};
use candle_nn::VarBuilder;
use lunaris_core::{LunarisError, StorageError};
use lunaris_rerank::{RerankCandidate, Reranker};

use crate::config::{ConfigError, XlmRobertaRerankerConfig};

/// O-01-E — Public-API rerank-pair batch ceiling. The reranker scores
/// (query, doc) pairs; we re-chunk the doc list into windows of this size
/// before dispatch so each forward pass touches at most `MAX_PUBLIC_BATCH`
/// pair tokenizations. Same Metal activation-footprint rationale as the
/// embedder's `MAX_PUBLIC_BATCH`. The HARDWARE-OPTIMIZATION-ROADMAP gate
/// table specifies p50 at K=10 for rerank; this ceiling at 8 means K=10
/// fits in two chunks (8 + 2) with no per-chunk fixed cost.
pub const MAX_PUBLIC_BATCH: usize = 8;

/// Pure resolver for the effective re-chunk ceiling from a raw env value.
/// Split out from [`public_batch_size`] so the fallback policy is unit-testable
/// without touching process env (avoids the global-env test race). Mirrors
/// `lunaris_embed_native::embedder::resolve_batch_size` byte-for-byte — same
/// fallback policy on both sides of the embed/rerank split.
///
/// Design-for-failure: a missing, non-numeric, or zero/negative value yields
/// the safe default [`MAX_PUBLIC_BATCH`] rather than panicking or producing a
/// 0-window (`slice::chunks(0)` panics).
fn resolve_batch_size(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok()).filter(|&n| n >= 1).unwrap_or(MAX_PUBLIC_BATCH)
}

/// Effective per-forward rerank-pair re-chunk ceiling. Defaults to
/// [`MAX_PUBLIC_BATCH`] (8) but operators on memory-rich hosts can raise it via
/// `LUNARIS_RERANK_BATCH` to better saturate the GPU on bulk rerank (mirrors
/// the embedder's `LUNARIS_EMBED_BATCH` / `public_batch_size`). Read once and
/// cached for the process lifetime.
pub fn public_batch_size() -> usize {
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| resolve_batch_size(std::env::var("LUNARIS_RERANK_BATCH").ok().as_deref()))
}

/// Maximum tolerated padding fraction per forward window — mirrors
/// `lunaris_embed_native::embedder::{MAX_PAD_FRACTION_NUM,_DEN}` byte-for-byte
/// (same policy on both sides of the embed/rerank split). The §4b profiling
/// matrix measured 48% padding waste on batch=8 rerank pairs, making batch=8
/// ~3× slower per pair than batch=1.
pub(crate) const MAX_PAD_FRACTION_NUM: u128 = 1;
pub(crate) const MAX_PAD_FRACTION_DEN: u128 = 4;

/// Length-bucketed batch plan (§4b-RESULTS finding #1) — mirrors
/// `lunaris_embed_native::embedder::plan_bucketed_batches` byte-for-byte
/// except the doc-comment. Rerank callers pass `u128::MAX` as `budget` (the
/// activation-footprint ceiling is an embedder concern; pair length is
/// already bounded by the tokenizer's `max_len` truncation).
///
/// Returns `(order, sizes)`: `order` is the input indices stably sorted by
/// byte length; `sizes` are forward-window row counts over that sorted
/// order. Every window satisfies `rows ≤ max_rows`,
/// `rows × max_len_in_window² ≤ budget`, and padding ≤
/// [`MAX_PAD_FRACTION_NUM`]/[`MAX_PAD_FRACTION_DEN`] of the window's padded
/// area. Callers MUST scatter window outputs back through `order`.
pub(crate) fn plan_bucketed_batches(
    byte_lens: &[usize],
    max_rows: usize,
    budget: u128,
) -> (Vec<usize>, Vec<usize>) {
    let max_rows = max_rows.max(1);
    let mut order: Vec<usize> = (0..byte_lens.len()).collect();
    order.sort_by_key(|&i| byte_lens[i]);

    let mut sizes: Vec<usize> = Vec::new();
    let mut count: usize = 0;
    let mut max_len: usize = 0;
    let mut sum_len: u128 = 0;
    for &i in &order {
        let len = byte_lens[i];
        if count >= 1 {
            let rows = count as u128 + 1;
            let pmax = max_len.max(len) as u128;
            let footprint = rows * pmax * pmax;
            let padded_area = rows * pmax;
            let pad_cells = padded_area - (sum_len + len as u128);
            if count >= max_rows
                || footprint > budget
                || pad_cells * MAX_PAD_FRACTION_DEN > padded_area * MAX_PAD_FRACTION_NUM
            {
                sizes.push(count);
                count = 0;
                max_len = 0;
                sum_len = 0;
            }
        }
        count += 1;
        max_len = max_len.max(len);
        sum_len += len as u128;
    }
    if count > 0 {
        sizes.push(count);
    }
    (order, sizes)
}
use crate::tokenizer::{EncodedPairBatch, PairTokenizer, TokenizerError};
use crate::xlmr_reranker::{ForwardError, XlmRobertaReranker};

/// Public construction options.
///
/// All three paths are required; defaults are not provided because there's no
/// canonical on-disk layout for bge-reranker-v2-m3 (it lives wherever the
/// operator downloaded it). Tests use the env-var convention
/// `BGE_RERANKER_WEIGHTS_PATH` / `BGE_RERANKER_TOKENIZER_PATH` /
/// `BGE_RERANKER_CONFIG_PATH` to point at the local cache.
#[derive(Clone, Debug)]
pub struct NativeRerankerOpts {
    pub weights_path: PathBuf,
    pub tokenizer_path: PathBuf,
    pub config_path: PathBuf,
    pub device: Device,
}

/// Errors raised during construction. Hot-path errors are mapped to
/// `LunarisError` via the trait surface; this enum exists so callers can
/// inspect / log specific variants before falling back.
#[derive(Debug, thiserror::Error)]
pub enum NativeRerankerError {
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

impl From<NativeRerankerError> for LunarisError {
    fn from(e: NativeRerankerError) -> Self {
        LunarisError::Storage(StorageError::Backend(format!("lunaris-rerank-native: {e}")))
    }
}

/// bge-reranker-v2-m3 native reranker. Cheap to clone — the heavy state lives
/// behind an `Arc<Inner>`.
#[derive(Clone)]
pub struct NativeReranker {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for NativeReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeReranker").field("max_len", &self.inner.tokenizer.max_len()).finish()
    }
}

struct Inner {
    /// The cross-encoder forward-pass model. candle's
    /// `XLMRobertaForSequenceClassification::forward` takes `&self`; concurrent
    /// `rerank` calls run truly in parallel (each on a `spawn_blocking`
    /// thread). CLAUDE.md lock-across-await constraint is satisfied vacuously:
    /// no lock is acquired on the hot path.
    model: XlmRobertaReranker,
    tokenizer: PairTokenizer,
    device: Device,
}

impl NativeReranker {
    /// Construct from on-disk artifacts. Synchronous I/O happens inline
    /// (reads safetensors, parses tokenizer, loads weights into the chosen
    /// `Device`). Callers concerned about runtime stalls should wrap in
    /// `tokio::task::spawn_blocking`; the load path itself is not async
    /// because error mapping stays straightforward this way.
    pub fn open(opts: NativeRerankerOpts) -> Result<Self, NativeRerankerError> {
        // O-01-B — physical-core rayon pool init. Idempotent with the
        // embedder side's call (shared global). No-op on Metal/CUDA.
        crate::rayon_pool::ensure_physical_core_pool();

        // O-01-C/D — Device upgrade.
        let mut opts = opts;
        opts.device = crate::device_select::select_device(opts.device);

        let cfg = XlmRobertaRerankerConfig::try_from_json_path(&opts.config_path)?;
        let tokenizer = PairTokenizer::from_file(&opts.tokenizer_path, cfg.pad_token_id)?;

        tracing::info!(
            backend = "lunaris-rerank-native",
            model = "bge-reranker-v2-m3",
            weights = %opts.weights_path.display(),
            "native reranker loading"
        );

        // Compute in FP32. Match the embedder side's rationale: the
        // classification head's tanh + sigmoid are sensitive enough that
        // fp16 leaks measurable drift on the cross-encoder calibration. The
        // Q4_K_M follow-up (N-02 step 2) re-evaluates this trade-off.
        let vb = candle_safetensors_varbuilder(&opts.weights_path, &opts.device)?;
        let model = XlmRobertaReranker::load(vb, &cfg)?;

        tracing::info!(
            backend = "lunaris-rerank-native",
            model = "bge-reranker-v2-m3",
            "native reranker initialized"
        );

        // O-01-C — warm-up matmul on the selected device.
        crate::device_select::warmup_device(&opts.device);

        Ok(Self { inner: Arc::new(Inner { model, tokenizer, device: opts.device }) })
    }

    /// Synchronous score path — for tests / direct callers that already have
    /// the inputs marshalled. The async trait method wraps this in
    /// `spawn_blocking`.
    ///
    /// Returns scores in input order (NOT sorted) — the public `rerank`
    /// method sorts after pairing scores with their candidates.
    pub fn score_blocking(&self, pairs: &[(&str, &str)]) -> Result<Vec<f32>, NativeRerankerError> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }

        // -- tokenize-pairs + batch-assembly ---------------------------------
        // `tokenizers::Tokenizer::encode_batch` fuses per-pair tokenization
        // with pad-to-batch-longest assembly in one call (see
        // `lunaris_embed_native::embedder::NativeEmbedder::embed_blocking` for
        // the full rationale — same fusion, same reason we can't split
        // further). Field values are only computed `if !span.is_disabled()`:
        // the mask reduction in `mask_token_stats` is real device compute.
        let tokenize_span = tracing::trace_span!(
            "lunaris.rerank.tokenize_pairs",
            batch_size = pairs.len(),
            max_seq_len = tracing::field::Empty,
            real_tokens = tracing::field::Empty,
            padded_tokens = tracing::field::Empty,
            quant = "fp32",
        );
        let EncodedPairBatch { input_ids, attention_mask, token_type_ids } = {
            let _enter = tokenize_span.enter();
            self.inner.tokenizer.encode_pair_batch(pairs, &self.inner.device)?
        };
        if !tokenize_span.is_disabled() {
            if let Ok(seq_len) = attention_mask.dim(1) {
                tokenize_span.record("max_seq_len", seq_len);
            }
            if let Ok((real, padded)) = crate::tokenizer::mask_token_stats(&attention_mask) {
                tokenize_span.record("real_tokens", real);
                tokenize_span.record("padded_tokens", padded);
            }
        }

        let scores = self.inner.model.score(&input_ids, &attention_mask, &token_type_ids)?;

        let copy_span =
            tracing::trace_span!("lunaris.rerank.copy", batch_size = pairs.len(), quant = "fp32");
        let out = {
            let _enter = copy_span.enter();
            scores.to_vec1::<f32>()?
        };
        Ok(out)
    }
}

/// Build a VarBuilder reading safetensors from disk in FP32 via a buffered
/// (non-mmap) loader. CLAUDE.md `#![forbid(unsafe_code)]` rules out
/// `VarBuilder::from_mmaped_safetensors` (which is `unsafe fn`); the
/// `from_buffered_safetensors` path is safe — it reads bytes into a `Vec` up
/// front, then candle decodes tensors lazily as `VarBuilder::get` is called.
///
/// Trade-off: ~1.1 GB transient RSS during load. Acceptable for the v0.4
/// milestone; the FP32 compute dtype means steady-state RSS is ~2.3 GB
/// regardless. The Q4 follow-up reconsiders both knobs together.
fn candle_safetensors_varbuilder(
    path: &std::path::Path,
    device: &Device,
) -> Result<VarBuilder<'static>, NativeRerankerError> {
    let bytes = std::fs::read(path).map_err(|e| {
        NativeRerankerError::Weights(format!(
            "safetensors read from {} failed: {e}",
            path.display()
        ))
    })?;
    VarBuilder::from_buffered_safetensors(bytes, DType::F32, device).map_err(|e| {
        NativeRerankerError::Weights(format!(
            "safetensors decode from {} failed: {e}",
            path.display()
        ))
    })
}

#[async_trait]
impl Reranker for NativeReranker {
    fn applies(&self) -> bool {
        true
    }

    async fn rerank(
        &self,
        query: &str,
        docs: Vec<RerankCandidate>,
    ) -> Result<Vec<RerankCandidate>, LunarisError> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }

        // Move owned inputs across the blocking boundary; `&str` borrows are
        // not `'static`.
        let owned_query = query.to_string();
        let me = self.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<RerankCandidate>, LunarisError> {
            // O-01-E + §4b length bucketing — re-chunk docs into windows of at
            // most public_batch_size() pairs (default MAX_PUBLIC_BATCH=8,
            // override via LUNARIS_RERANK_BATCH), walking a length-sorted
            // order so a window never mixes a short doc with a long one (the
            // §4b matrix measured 48% padding waste on mixed batch=8 pairs).
            // Scores scatter back through `order` so `all_scores[i]` is
            // docs[i]'s score — the zip below pairs them positionally. Pair
            // length includes the constant query so the waste ceiling sees
            // the real padded sequence.
            let lens: Vec<usize> = docs.iter().map(|d| owned_query.len() + d.text.len()).collect();
            let (order, sizes) = plan_bucketed_batches(&lens, public_batch_size(), u128::MAX);
            let mut all_scores: Vec<f32> = vec![f32::NAN; docs.len()];
            let mut start = 0usize;
            for sz in sizes {
                let idxs = &order[start..start + sz];
                let pairs: Vec<(&str, &str)> =
                    idxs.iter().map(|&i| (owned_query.as_str(), docs[i].text.as_str())).collect();
                let scores = me.score_blocking(&pairs).map_err(LunarisError::from)?;
                if scores.len() != sz {
                    return Err(LunarisError::Storage(StorageError::Backend(format!(
                        "lunaris-rerank-native: score_blocking returned {} scores for {sz} pairs",
                        scores.len()
                    ))));
                }
                for (&i, s) in idxs.iter().zip(scores) {
                    all_scores[i] = s;
                }
                start += sz;
            }

            // Pair scores with docs, mutate score in place, sort desc.
            let mut scored: Vec<(f32, RerankCandidate)> = all_scores
                .into_iter()
                .zip(docs)
                .map(|(s, mut d)| {
                    d.score = s;
                    (s, d)
                })
                .collect();
            // NaN sentinel: bubble NaNs to the bottom so an isolated forward-pass
            // hiccup doesn't poison the top-k slot. partial_cmp returns None on
            // NaN; treat that as "less than" the other side.
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            Ok(scored.into_iter().map(|(_, d)| d).collect())
        })
        .await
        .map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("lunaris-rerank-native join: {e}")))
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trait dyn-compat: `NativeReranker` MUST be usable as `Arc<dyn Reranker>`
    // (the public Lunaris construction pattern).
    #[allow(dead_code)]
    fn _dyn_compat(r: NativeReranker) -> std::sync::Arc<dyn Reranker> {
        std::sync::Arc::new(r)
    }

    #[test]
    fn open_with_missing_config_fails_fast() {
        let opts = NativeRerankerOpts {
            weights_path: PathBuf::from("/tmp/__lunaris_no_weights.safetensors"),
            tokenizer_path: PathBuf::from("/tmp/__lunaris_no_tokenizer.json"),
            config_path: PathBuf::from("/tmp/__lunaris_no_config.json"),
            device: Device::Cpu,
        };
        let err = NativeReranker::open(opts).expect_err("missing files must fail");
        match err {
            NativeRerankerError::Config(_) => {}
            other => panic!("expected Config error, got: {other:?}"),
        }
    }

    // -- embed-accel-passthrough task: LUNARIS_RERANK_BATCH resolver --------
    // Mirrors lunaris-embed-native::embedder::{resolve_batch_size,
    // public_batch_size} 1:1 (same fallback policy, same pure-function shape
    // to avoid the global-env test race — see that module's doc comment).
    // RED until `resolve_batch_size` / `public_batch_size` exist in this crate.

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
        assert_eq!(resolve_batch_size(Some("0")), MAX_PUBLIC_BATCH);
        assert_eq!(resolve_batch_size(Some("-4")), MAX_PUBLIC_BATCH);
        assert_eq!(resolve_batch_size(Some("abc")), MAX_PUBLIC_BATCH);
        assert_eq!(resolve_batch_size(Some("")), MAX_PUBLIC_BATCH);
    }

    // §4b length bucketing — `plan_bucketed_batches` (mirror of the
    // lunaris-embed-native tests, rerank operating point: budget=u128::MAX).
    // RED observed as E0425 (helper absent) before the planner landed.

    fn assert_bucketed_invariants(
        lens: &[usize],
        order: &[usize],
        sizes: &[usize],
        max_rows: usize,
    ) {
        let mut seen = order.to_vec();
        seen.sort_unstable();
        assert_eq!(seen, (0..lens.len()).collect::<Vec<_>>(), "order must be a permutation");
        assert_eq!(sizes.iter().sum::<usize>(), lens.len(), "sizes must cover every input");

        let mut start = 0usize;
        for &sz in sizes {
            let window: Vec<usize> = order[start..start + sz].iter().map(|&i| lens[i]).collect();
            let rows = sz as u128;
            let pmax = *window.iter().max().unwrap() as u128;
            let sum: u128 = window.iter().map(|&l| l as u128).sum();
            assert!(sz <= max_rows.max(1), "rows {sz} > max_rows {max_rows}");
            let padded_area = rows * pmax;
            assert!(
                (padded_area - sum) * MAX_PAD_FRACTION_DEN <= padded_area * MAX_PAD_FRACTION_NUM,
                "padding waste over ceiling in window {window:?}"
            );
            start += sz;
        }
    }

    /// THE §4b REGRESSION GUARD (rerank side). Interleaved short/long pairs
    /// (the shape the profiling matrix measured at 48% padding waste) must
    /// split into length-homogeneous windows.
    #[test]
    fn bucketed_plan_separates_interleaved_pair_lengths() {
        // Pair lens = query + doc bytes; constant query offset (60) applied.
        let lens = vec![74usize, 1260, 74, 1260, 74, 1260];
        let (order, sizes) = plan_bucketed_batches(&lens, 8, u128::MAX);
        assert_eq!(sizes, vec![3, 3], "short and long pairs must land in separate windows");
        assert_eq!(&order[..3], &[0, 2, 4], "equal lengths keep input order (stable sort)");
        assert_eq!(&order[3..], &[1, 3, 5]);
        assert_bucketed_invariants(&lens, &order, &sizes, 8);
    }

    #[test]
    fn bucketed_plan_identical_lengths_fill_to_row_cap() {
        // K=10 at the default row cap of 8 — the rerank gate's test point —
        // must still be two windows (8 + 2), same as the contiguous chunking.
        let lens = vec![300usize; 10];
        let (order, sizes) = plan_bucketed_batches(&lens, 8, u128::MAX);
        assert_eq!(sizes, vec![8, 2]);
        assert_eq!(order, (0..10).collect::<Vec<_>>(), "stable sort keeps identity order");
        assert_bucketed_invariants(&lens, &order, &sizes, 8);
    }

    #[test]
    fn bucketed_plan_smooth_ramp_does_not_fragment() {
        let lens: Vec<usize> = (0..16).map(|i| 400 + i * 10).collect();
        let (order, sizes) = plan_bucketed_batches(&lens, 8, u128::MAX);
        assert_eq!(sizes, vec![8, 8], "a smooth ramp fills windows to the row cap");
        assert_bucketed_invariants(&lens, &order, &sizes, 8);
    }

    #[test]
    fn bucketed_plan_empty_singleton_and_zero_rows() {
        let (order, sizes) = plan_bucketed_batches(&[], 8, u128::MAX);
        assert!(order.is_empty() && sizes.is_empty());
        let (order, sizes) = plan_bucketed_batches(&[5], 8, u128::MAX);
        assert_eq!((order, sizes), (vec![0], vec![1]));
        let (_, sizes) = plan_bucketed_batches(&[10, 10, 10], 0, u128::MAX);
        assert_eq!(sizes, vec![1, 1, 1]);
    }

    #[test]
    fn bucketed_plan_zero_length_inputs_do_not_divide_by_zero() {
        let lens = vec![0usize, 0, 0, 500];
        let (order, sizes) = plan_bucketed_batches(&lens, 8, u128::MAX);
        assert_bucketed_invariants(&lens, &order, &sizes, 8);
    }
}
