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
    raw.and_then(|v| v.trim().parse::<usize>().ok()).filter(|&n| n >= 1).unwrap_or(MAX_PUBLIC_BATCH)
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

/// Reference "normal" sequence length, in **input bytes**, that the count-based
/// batch ceiling ([`public_batch_size`]) was calibrated against (~512 tokens at
/// ~4 bytes/token). The activation budget is `public_batch_size × REF²` so that
/// a batch of `public_batch_size` reference-length inputs is the worst-case
/// footprint we ever schedule — and longer inputs get proportionally FEWER rows
/// to hold that same footprint constant.
pub(crate) const EMBED_REF_SEQ_BYTES: usize = 2048;
/// Activation-footprint budget in `rows × bytes²` units. The transformer's
/// attention scratch scales as `rows × seq²`; bounding `rows × max_seq_bytes²`
/// per forward pass therefore caps peak activation memory **regardless of input
/// length** — closing the count-only ceiling's blind spot where a batch of long
/// inputs (e.g. RAPTOR community summaries) padded a `[rows, heads, 8192, 8192]`
/// attention tensor to tens/hundreds of GB and OOM-killed ingest (and crashed
/// Metal's buffer pool). Bytes (not tokens) is a conservative proxy: bytes ≥
/// tokens, so the budget never under-counts the real sequence length.
pub(crate) fn activation_budget() -> u128 {
    (public_batch_size() as u128) * (EMBED_REF_SEQ_BYTES as u128).pow(2)
}

/// Maximum tolerated padding fraction per forward window: padded cells
/// (`rows × max_len − Σ len`) must stay ≤ 1/4 of the padded area
/// (`rows × max_len`). The §4b profiling matrix measured 67% padding waste
/// under pad-to-longest at batch=8, making batch=8 *slower* than batch=1 in
/// every device × quant cell — this ceiling is what makes batching a win
/// again instead of a tax.
pub(crate) const MAX_PAD_FRACTION_NUM: u128 = 1;
pub(crate) const MAX_PAD_FRACTION_DEN: u128 = 4;

/// Length-bucketed batch plan (§4b-RESULTS finding #1).
///
/// Returns `(order, sizes)`: `order` is the input indices stably sorted by
/// byte length (equal-length inputs keep their relative order, so the plan
/// is deterministic), and `sizes` are forward-window row counts over that
/// sorted order. Every window satisfies THREE ceilings:
///   1. `rows ≤ max_rows` (the count cap / `LUNARIS_EMBED_BATCH`),
///   2. `rows × max_len_in_window² ≤ budget` (the activation-footprint cap —
///      the RAPTOR-summary ~124 GB OOM guard; a lone over-budget input still
///      forms a window of exactly one, the irreducible floor — the tokenizer
///      separately truncates it to `max_position_embeddings`), and
///   3. padding ≤ [`MAX_PAD_FRACTION_NUM`]/[`MAX_PAD_FRACTION_DEN`] of the
///      window's padded area (the padding-waste cap — new here).
///
/// Because the walk is over sorted lengths, ceiling 3 only fires at genuine
/// length jumps (a 16-token memo next to a 512-token summary); a smoothly
/// increasing corpus fills windows to `max_rows` exactly like the contiguous
/// planner. Callers MUST scatter window outputs back through `order` — the
/// window walk no longer visits inputs in input order.
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

        // -- tokenize + batch-assembly ---------------------------------------
        // `tokenizers::Tokenizer::encode_batch` fuses per-string tokenization
        // with pad-to-batch-longest assembly in one call — the two stages the
        // microscope plan names (§4b) can't be split further without reaching
        // into that crate's internals. The span still carries the
        // batch-assembly measurement payload (real_tokens/padded_tokens
        // recorded AFTER the call) so padding waste is visible. Field values
        // are only computed `if !span.is_disabled()` — the mask reduction in
        // `mask_token_stats` is real device compute, not free, so we skip it
        // entirely when nothing is subscribed (near-zero-cost-when-disabled).
        let tokenize_span = tracing::trace_span!(
            "lunaris.embed.tokenize",
            batch_size = inputs.len(),
            max_seq_len = tracing::field::Empty,
            real_tokens = tracing::field::Empty,
            padded_tokens = tracing::field::Empty,
        );
        let EncodedBatch { input_ids, attention_mask } = {
            let _enter = tokenize_span.enter();
            self.inner.tokenizer.encode_batch(inputs, &self.inner.device)?
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

        let pooled = pooled_forward(&self.inner.model, &input_ids, &attention_mask)?;

        let copy_span = tracing::trace_span!("lunaris.embed.copy", batch_size = inputs.len());
        let rows: Vec<Vec<f32>> = {
            let _enter = copy_span.enter();
            pooled.to_vec2::<f32>()?
        };
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
            // O-01-E + activation-budget + §4b length bucketing — re-chunk user
            // input so each forward pass is bounded by row count
            // (`public_batch_size`), by the `rows × max_seq²` activation
            // footprint (the RAPTOR-summary 124 GB OOM guard), AND by padding
            // waste (`plan_bucketed_batches` walks a length-sorted order so a
            // window never mixes a short memo with a long summary — the §4b
            // matrix measured pad-to-longest making batch=8 slower than
            // batch=1). Each window runs through the same `embed_blocking`
            // path; rows are scattered back through `order` so `out[i]` is the
            // embedding of `inputs[i]` exactly as before. For ≤8 same-length
            // inputs this is a single window = single forward = zero overhead.
            let lens: Vec<usize> = owned.iter().map(|s| s.len()).collect();
            let (order, sizes) =
                plan_bucketed_batches(&lens, public_batch_size(), activation_budget());
            let mut out: Vec<Vec<f32>> = vec![Vec::new(); owned.len()];
            let mut start = 0usize;
            for sz in sizes {
                let idxs = &order[start..start + sz];
                let refs: Vec<&str> = idxs.iter().map(|&i| owned[i].as_str()).collect();
                let rows = me.embed_blocking(&refs).map_err(LunarisError::from)?;
                if rows.len() != sz {
                    return Err(LunarisError::Storage(StorageError::Backend(format!(
                        "lunaris-embed-native: embed_blocking returned {} rows for {sz} inputs",
                        rows.len()
                    ))));
                }
                for (&i, row) in idxs.iter().zip(rows) {
                    out[i] = row;
                }
                start += sz;
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

    /// Test-only budget that does not read process env (avoids the global-env
    /// race the `resolve_batch_size` tests document).
    fn activation_budget_for(rows: usize) -> u128 {
        (rows as u128) * (EMBED_REF_SEQ_BYTES as u128).pow(2)
    }

    // §4b length bucketing — `plan_bucketed_batches`. RED observed as E0425
    // (helper absent) before the planner landed. The activation-budget cases
    // below carry over the OOM-fix regression coverage from the deleted
    // contiguous `plan_batches` planner (this planner supersedes it at every
    // call site).

    /// Every window over the sorted order must satisfy all three ceilings.
    fn assert_bucketed_invariants(
        lens: &[usize],
        order: &[usize],
        sizes: &[usize],
        max_rows: usize,
        budget: u128,
    ) {
        // `order` is a permutation of 0..n and `sizes` covers it exactly.
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
            // Singleton windows are the irreducible floor — a lone input may
            // exceed the budget by design (tokenizer truncation bounds it).
            assert!(sz == 1 || rows * pmax * pmax <= budget, "footprint over budget: {window:?}");
            let padded_area = rows * pmax;
            assert!(
                (padded_area - sum) * MAX_PAD_FRACTION_DEN <= padded_area * MAX_PAD_FRACTION_NUM,
                "padding waste over ceiling in window {window:?}"
            );
            start += sz;
        }
    }

    /// THE §4b REGRESSION GUARD. An interleaved short/long corpus (the shape
    /// the profiling matrix measured at 67% padding waste) must split into
    /// length-homogeneous windows, never one mixed pad-to-longest window.
    #[test]
    fn bucketed_plan_separates_interleaved_lengths() {
        let lens = vec![40usize, 1200, 40, 1200, 40, 1200];
        let budget = activation_budget_for(8);
        let (order, sizes) = plan_bucketed_batches(&lens, 8, budget);
        assert_eq!(sizes, vec![3, 3], "shorts and longs must land in separate windows");
        // Sorted walk puts the three shorts first, stably (0, 2, 4).
        assert_eq!(&order[..3], &[0, 2, 4], "equal lengths keep input order (stable sort)");
        assert_eq!(&order[3..], &[1, 3, 5]);
        assert_bucketed_invariants(&lens, &order, &sizes, 8, budget);
    }

    #[test]
    fn bucketed_plan_identical_lengths_fill_to_row_cap() {
        // No length variance ⇒ the waste ceiling never fires; behaves exactly
        // like the contiguous planner (8+8+4) with an identity-ish order.
        let lens = vec![100usize; 20];
        let budget = activation_budget_for(8);
        let (order, sizes) = plan_bucketed_batches(&lens, 8, budget);
        assert_eq!(sizes, vec![8, 8, 4]);
        assert_eq!(order, (0..20).collect::<Vec<_>>(), "stable sort keeps identity order");
        assert_bucketed_invariants(&lens, &order, &sizes, 8, budget);
    }

    #[test]
    fn bucketed_plan_smooth_ramp_does_not_fragment() {
        // Smoothly increasing lengths (±10%) must NOT split on the waste
        // ceiling — bucketing only pays at genuine length jumps.
        let lens: Vec<usize> = (0..16).map(|i| 400 + i * 10).collect();
        let budget = activation_budget_for(8);
        let (order, sizes) = plan_bucketed_batches(&lens, 8, budget);
        assert_eq!(sizes, vec![8, 8], "a smooth ramp fills windows to the row cap");
        assert_bucketed_invariants(&lens, &order, &sizes, 8, budget);
    }

    /// The RAPTOR-summary OOM guard carries over: a single very long input
    /// still lands in a window of exactly one, even sorted to the end.
    #[test]
    fn bucketed_plan_preserves_the_oom_guard() {
        let mut lens = vec![200usize; 5];
        lens.insert(2, 32_768);
        let budget = activation_budget_for(32);
        let (order, sizes) = plan_bucketed_batches(&lens, 32, budget);
        assert_eq!(*order.last().unwrap(), 2, "the long input sorts to the end");
        assert_eq!(*sizes.last().unwrap(), 1, "the long input must sit alone");
        assert_bucketed_invariants(&lens, &order, &sizes, 32, budget);
    }

    #[test]
    fn bucketed_plan_medium_inputs_get_fewer_rows() {
        // Inputs at 2× the reference length → ~1/4 the rows of the count cap
        // (the activation budget, not the row cap, is binding). Ported from
        // the deleted contiguous planner's regression suite.
        let lens = vec![EMBED_REF_SEQ_BYTES * 2; 16];
        let budget = activation_budget_for(8);
        let (order, sizes) = plan_bucketed_batches(&lens, 8, budget);
        assert!(sizes.iter().all(|&s| s <= 2), "2× length ⇒ ≤2 rows/batch: {sizes:?}");
        assert_bucketed_invariants(&lens, &order, &sizes, 8, budget);
    }

    #[test]
    fn bucketed_plan_empty_singleton_and_zero_rows() {
        let budget = activation_budget_for(8);
        let (order, sizes) = plan_bucketed_batches(&[], 8, budget);
        assert!(order.is_empty() && sizes.is_empty());
        let (order, sizes) = plan_bucketed_batches(&[5], 8, budget);
        assert_eq!((order, sizes), (vec![0], vec![1]));
        // A misconfigured ceiling of 0 must clamp to 1, never panic / 0-window.
        let (_, sizes) = plan_bucketed_batches(&[10, 10, 10], 0, budget);
        assert_eq!(sizes, vec![1, 1, 1]);
    }

    #[test]
    fn bucketed_plan_zero_length_inputs_do_not_divide_by_zero() {
        // Empty strings ⇒ padded_area = 0; the waste rule must not panic and
        // the plan must still cover every input.
        let lens = vec![0usize, 0, 0, 500];
        let budget = activation_budget_for(8);
        let (order, sizes) = plan_bucketed_batches(&lens, 8, budget);
        assert_bucketed_invariants(&lens, &order, &sizes, 8, budget);
    }
}
