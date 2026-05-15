//! `lunaris-embed-mlx-spike` — O-02 spike crate.
//!
//! This crate is **throwaway** (`publish = false`, version `0.0.0`). It exists
//! to answer one question for the v0.4 hardware-optimisation roadmap: does an
//! MLX-backed ModernBERT layer beat a candle-Metal ModernBERT layer on Apple
//! Silicon by ≥1.5× while preserving numerical fidelity?
//!
//! Scope (deliberately narrow):
//!
//! - One ModernBERT encoder layer (RoPE attention → residual → RMSNorm →
//!   GeGLU MLP → residual → RMSNorm), implemented twice — once in `mlx-rs`
//!   and once in `candle-core` + `candle-nn`. **Both** implementations are
//!   fed the same FP16 random-init weights derived from a seeded RNG so the
//!   math is identical.
//! - A numerical-equivalence test (`tests/numerical_equivalence.rs`)
//!   computes mean-cosine drift between the two layer outputs on 50 random
//!   inputs.
//! - A Criterion bench (`benches/single_layer.rs`) measures 1000 forward
//!   passes per backend on Apple Silicon.
//!
//! Out of scope for the spike: real granite-r2 weights (none staged on
//! the spike host; the synthetic-weights path measures kernel-level perf
//! and kernel-level drift, which is what the GO/NO-GO contract turns on).
//! See `docs/spike/O-02-mlx/MATURITY-AUDIT.md` §5 and DECISION.md for the
//! reasoning.

#![deny(rust_2018_idioms)]
// Allow new-style explicit lifetime captures since edition 2024 enforces them.
#![allow(clippy::needless_lifetimes)]

pub mod hello;
pub mod layer_candle;
pub mod layer_mlx;
pub mod params;

pub use hello::mlx_hello_world;
pub use params::{LayerParams, granite_r2_hparams};
