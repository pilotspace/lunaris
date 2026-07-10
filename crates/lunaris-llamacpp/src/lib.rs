//! lunaris-llamacpp — opt-in llama.cpp inference runtime.
//!
//! ADR: `docs/decisions/2026-07-10-llamacpp-inference-runtime.md`, extended
//! by the 2026-07-10 runtime-cutover decision (llama.cpp becomes the ONLY
//! embed/rerank runtime; candle is deleted in cutover Phase C). Ships
//! [`LlamaCppEmbedder`] (granite-embedding-311m GGUF) and
//! [`LlamaCppReranker`] (bge-reranker-v2-m3 GGUF, cutover Phase A1); the
//! extractor/verifier slots go remote-only and never land here.
//!
//! Everything hangs off the `llamacpp` feature. With it OFF (the default
//! until cutover Phase B flips the umbrella), this crate is an empty
//! shell: no `llama-cpp-sys-2`, no cmake, no C++ toolchain. This is the
//! `embedded-moon` discipline applied to an inference backend.

#[cfg(feature = "llamacpp")]
mod backend;
#[cfg(feature = "llamacpp")]
mod embedder;
#[cfg(feature = "llamacpp")]
mod gguf_head;
#[cfg(feature = "llamacpp")]
mod reranker;
#[cfg(feature = "llamacpp")]
mod worker;

#[cfg(feature = "llamacpp")]
pub use embedder::{LlamaCppEmbedder, LlamaCppEmbedderError, LlamaCppEmbedderOpts};
#[cfg(feature = "llamacpp")]
pub use gguf_head::GgufHeadError;
#[cfg(feature = "llamacpp")]
pub use reranker::{LlamaCppReranker, LlamaCppRerankerError, LlamaCppRerankerOpts};
