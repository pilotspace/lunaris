//! lunaris-llamacpp — opt-in llama.cpp inference runtime.
//!
//! ADR: `docs/decisions/2026-07-10-llamacpp-inference-runtime.md`. Spike
//! scope is the embedder only ([`LlamaCppEmbedder`], granite-embedding-311m
//! GGUF); reranker + extractor are follow-ups behind the same feature.
//!
//! Everything hangs off the `llamacpp` feature. With it OFF (the default
//! everywhere — workspace builds, CI, published binaries), this crate is an
//! empty shell: no `llama-cpp-sys-2`, no cmake, no C++ toolchain. This is
//! the `embedded-moon` discipline applied to an inference backend.

#[cfg(feature = "llamacpp")]
mod embedder;

#[cfg(feature = "llamacpp")]
pub use embedder::{LlamaCppEmbedder, LlamaCppEmbedderError, LlamaCppEmbedderOpts};
