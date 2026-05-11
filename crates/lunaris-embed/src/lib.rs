//! lunaris-embed — real `Embedder` impls for Phase 2 hot path (INGEST-02).
//!
//! - **Default backend** (`feature = "candle"`): `CandleEmbeddingGemma` — loads
//!   EmbeddingGemma 300M tokenizer + token-embedding matrix from a local cache,
//!   mean-pools the embedded token vectors per input, and L2-normalises to a 768-d
//!   unit vector. Falls back with an actionable `LunarisError::Storage` error
//!   when the weights cache is missing.
//! - **Alt backend** (`feature = "ollama"`): `OllamaEmbedder` — POSTs each batch
//!   to `<endpoint>/api/embed` (Ollama's `embed` HTTP API), validates the response
//!   shape against [`Embedder::dim`], and returns 768-d rows. 10s HTTP timeout
//!   (CLAUDE.md: "design for failure — timeouts").
//!
//! Phase 1's [`lunaris_core::StubEmbedder`] remains the deterministic test impl —
//! ingest tests inject it via the `Lunaris::with_embedder` escape hatch so they
//! don't pay model-load latency. Production callers get `CandleEmbeddingGemma`
//! by default through `Lunaris::open(url)` (Plan 02-01 Task 3).
//!
//! ## Latency budget swap escape hatch
//!
//! Per `02-01-PLAN.md` critical constraints: if candle local inference busts
//! the per-batch budget on the dev box (8ms p50 / 20ms p99 per blueprint §4.1),
//! callers swap to `OllamaEmbedder` via `Lunaris::with_embedder(Arc::new(...))`.
//! The trait shape does NOT change either way — that's the whole point of the
//! Phase 1 [`Embedder`] interface lock.
#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

#[cfg(feature = "candle")]
pub mod candle_gemma;
#[cfg(feature = "fastembed")]
pub mod fastembed;
#[cfg(feature = "fastembed")]
pub mod fastembed_exec;
#[cfg(feature = "ollama")]
pub mod ollama;

#[cfg(feature = "candle")]
pub use candle_gemma::{CandleEmbeddingGemma, CandleEmbeddingGemmaOpts};
#[cfg(feature = "fastembed")]
pub use fastembed::{
    FASTEMBED_EXECUTION_ENV, FastembedEmbedder, FastembedEmbedderOpts, FastembedUserDefinedOpts,
    PoolingMode,
};
#[cfg(feature = "fastembed")]
pub use fastembed_exec::{ExecutionPreference, execution_from_env, parse_execution};
#[cfg(feature = "ollama")]
pub use ollama::{OllamaEmbedder, OllamaEmbedderOpts};

// Re-export the trait from core so callers can `use lunaris_embed::Embedder` in
// one import alongside the concrete backends.
pub use lunaris_core::Embedder;
