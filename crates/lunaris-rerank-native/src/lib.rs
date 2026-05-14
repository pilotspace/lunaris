//! lunaris-rerank-native — native candle XLM-RoBERTa cross-encoder for
//! `BAAI/bge-reranker-v2-m3`.
//!
//! Milestone v0.4 N-02 step 1. Companion to `lunaris-embed-native` (N-01); the
//! pair forms the v0.4 native-model stack. The Q4_K_M quantized variant is
//! N-02 step 2 — out of scope here.
//!
//! ## Why a new crate?
//!
//! The existing `lunaris-rerank::BgeRerankerV2M3` (candle path, scheduled for
//! removal in v0.4) ships *raw logits* through the `Reranker` trait. v0.4 ships
//! sigmoid-bounded scores in `[0, 1]` so downstream RRF / score-thresholding
//! sees a calibrated relevance value. The new crate also adds the
//! NFC-normalize pre-pass that the v0.3 path was missing (the embedder added
//! it in N-01.5; the reranker matches).
//!
//! ## Public surface
//!
//! - [`NativeReranker`] — `lunaris_rerank::Reranker` impl (cheap-clone
//!   `Arc<Inner>`); `rerank(query, docs)` runs the cross-encoder forward pass
//!   and returns `docs` re-scored (sigmoid of single logit) and sorted by score
//!   descending.
//! - [`NativeRerankerOpts`] — construction options (paths + device).
//! - [`BGE_RERANKER_V2_M3_DIM`] — hidden width (1024).
//! - [`BGE_RERANKER_V2_M3_MAX_SEQ`] — pair-encoding cap (512 tokens; bge uses
//!   512 effective context even though XLM-R supports 8192).
//!
//! ## CLAUDE.md compliance
//!
//! - `#![forbid(unsafe_code)]` — uses the safe
//!   `VarBuilder::from_buffered_safetensors` loader, not the `unsafe` mmap one.
//! - No lock held across `.await` — the forward pass runs inside
//!   `tokio::task::spawn_blocking`; no `Mutex`/`RwLock` is acquired on the hot
//!   path (the `Arc<Inner>` provides shared immutable access).
//! - `parking_lot` is depended on for future scratch state but unused in the
//!   v0 forward path.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod config;
pub mod reranker;
pub mod tokenizer;
pub mod xlmr_reranker;

#[cfg(feature = "reranker-gguf")]
pub mod quantized_reranker;
#[cfg(feature = "reranker-gguf")]
pub mod quantized_xlmr;

pub use config::{ConfigError, XlmRobertaRerankerConfig};
pub use reranker::{NativeReranker, NativeRerankerError, NativeRerankerOpts};
pub use tokenizer::{EncodedPairBatch, PairTokenizer, TokenizerError};
pub use xlmr_reranker::{ForwardError, XlmRobertaReranker};

#[cfg(feature = "reranker-gguf")]
pub use quantized_reranker::{
    NativeQuantizedReranker, NativeQuantizedRerankerError, NativeQuantizedRerankerOpts,
};
#[cfg(feature = "reranker-gguf")]
pub use quantized_xlmr::{QuantizedRerankError, QuantizedXlmRoberta};

/// Hidden-state width of `BAAI/bge-reranker-v2-m3` (XLM-RoBERTa-large).
/// Public for downstream sanity-checks.
pub const BGE_RERANKER_V2_M3_DIM: usize = 1024;

/// SHA-256 of the canonical imatrix-calibrated **Q5_K_M** GGUF for
/// `BAAI/bge-reranker-v2-m3`, produced by
/// `scripts/spike-imatrix-bge-rerank.sh` (llama.cpp mainline `ccb9e9b7c` +
/// PR #22716). The constant name retains the historical `Q4` token for
/// API-compat; the actual quant level is recorded here in the docstring
/// and in the `Q4_K_M.sha256` artifact alongside the GGUF.
///
/// Producers should call `scripts/spike-imatrix-bge-rerank.sh` to
/// (re)produce this artifact; consumers should verify the SHA-256 of the
/// `.gguf` file at load time and refuse to proceed on mismatch.
///
/// File size at this pin: **446 MiB / 6.50 BPW** (imatrix Q5_K_M).
///
/// Why Q5_K_M and not plain Q4_K_M? The N-02 step 2 brief targeted
/// ≤ 320 MiB. Plain Q4_K_M (240 MiB / 4.53 BPW) and imatrix-Q4_K_M
/// (418 MiB / 6.08 BPW — `token_embd` auto-upgrades to Q6 when imatrix
/// doesn't cover it) both fail the ≤ 0.10 sigmoid-space drift gate on
/// rare-token / cross-lingual pairs (max delta 0.218 plain, 0.355
/// imatrix-Q4). The non-quantizable F32 floor (250 k vocab × 1024 +
/// 8192-row position table + per-tensor F32 biases on every linear)
/// makes the 320 MiB target unreachable without aggressive embedding
/// quant that drives drift > gate. Q5_K_M + imatrix at 446 MiB lands
/// the drift gate at max=0.0425 / mean=0.00421 — both well inside the
/// 0.10 / 0.04 contract. The size deviation vs the original 320 MiB
/// target is the cost of correctness on multilingual inputs.
pub const BGE_RERANKER_GGUF_Q4_SHA256: &str =
    "6cdcc566200dba69553a89a9d59ff6d631e33969bc9367eff6914919f7722a1c";

/// Max sequence length used for pair encoding `(query, doc)`. bge-reranker-v2-m3's
/// `tokenizer_config.json` ships `model_max_length: 8192`, but the released
/// model card recommends 512 for the cross-encoder rerank pass (longer inputs
/// blow the 12 ms p50 latency budget without measurable nDCG@10 gain). We pin
/// 512 here; callers that need a different cap can override via the tokenizer
/// path's truncation params if they take ownership of the inner state.
pub const BGE_RERANKER_V2_M3_MAX_SEQ: usize = 512;
