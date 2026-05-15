//! Shared synthetic weight panel + ModernBERT hyperparameters used by both
//! the candle reference and the MLX port.
//!
//! Two implementations must consume the **same** bytes so the
//! numerical-equivalence test is comparing math, not random-init noise.
//! That is why this module owns the `LayerParams` struct + the seeded RNG.

use rand::distributions::{Distribution, Uniform};
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;

/// Granite-r2 hyperparameters mirrored from
/// `lunaris-embed-native::ModernBertConfig::granite_r2()`. The spike uses a
/// global-attention layer (no local sliding-window mask) so we can rely on
/// MLX's fused `fast::scaled_dot_product_attention` without a custom bias.
#[derive(Debug, Clone, Copy)]
pub struct HParams {
    pub hidden_size: usize,
    pub num_heads: usize,
    pub intermediate_size: usize,
    pub seq_len: usize,
    pub batch: usize,
    pub rope_theta: f32,
    pub rms_eps: f32,
}

/// Pinned per the spike contract: batch=8 / seq=128 / hidden=768 / heads=12
/// matches the per-device bench panel (`crates/lunaris-bench/benches/per_device.rs`).
pub fn granite_r2_hparams() -> HParams {
    HParams {
        hidden_size: 768,
        num_heads: 12,
        intermediate_size: 1152,
        seq_len: 128,
        batch: 8,
        // Use the global-attention θ (granite-r2's larger one — handles 32k ctx).
        rope_theta: 150_000.0,
        rms_eps: 1e-12,
    }
}

/// All trainable parameters for one ModernBERT layer, stored as flat f32
/// vectors so the same bytes can be uploaded to candle and MLX without any
/// re-randomisation between backends.
#[derive(Debug, Clone)]
pub struct LayerParams {
    pub wqkv: Vec<f32>,
    pub wo: Vec<f32>,
    pub attn_norm: Vec<f32>,
    pub w_gate: Vec<f32>,
    pub w_up: Vec<f32>,
    pub w_down: Vec<f32>,
    pub mlp_norm: Vec<f32>,
}

impl LayerParams {
    /// Produce a deterministic random-init layer for hyperparameters `h`,
    /// seeded with `seed`.
    pub fn synthetic(h: HParams, seed: u64) -> Self {
        let mut rng = ChaCha20Rng::seed_from_u64(seed);
        let h_dim = h.hidden_size;
        let i_dim = h.intermediate_size;
        // LeCun-ish init so attention scores stay in the FP16 representable
        // range and softmax doesn't saturate.
        let scale = 1.0_f32 / (h_dim as f32).sqrt();
        let dist = Uniform::new(-scale, scale);
        let mut sample_n = |n: usize| -> Vec<f32> {
            (0..n).map(|_| dist.sample(&mut rng)).collect()
        };
        let ones = |n: usize| vec![1.0_f32; n];
        Self {
            wqkv: sample_n(3 * h_dim * h_dim),
            wo: sample_n(h_dim * h_dim),
            attn_norm: ones(h_dim),
            w_gate: sample_n(i_dim * h_dim),
            w_up: sample_n(i_dim * h_dim),
            w_down: sample_n(h_dim * i_dim),
            mlp_norm: ones(h_dim),
        }
    }
}

/// Produce a (`batch * seq_len * hidden_size`) input tensor as a flat
/// `Vec<f32>`. Deterministic given `seed`.
pub fn synthetic_input(h: HParams, seed: u64) -> Vec<f32> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let n = h.batch * h.seq_len * h.hidden_size;
    let dist = Uniform::new(-0.25_f32, 0.25_f32);
    (0..n).map(|_| dist.sample(&mut rng)).collect()
}
