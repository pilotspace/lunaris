//! candle-Metal reference implementation of the single ModernBERT
//! encoder layer. Mirrors the math in `candle_transformers::models::
//! modernbert::Layer` but inlined here so the spike's head-to-head is
//! kernel-vs-kernel rather than going through the production
//! transformer-rs wrapper (which would also load real weights).

use candle_core::{D, DType, Device, Result as CandleResult, Tensor};

use crate::params::{HParams, LayerParams};

/// One ModernBERT global-attention layer in candle-core.
///
/// Forward pipeline:
///   y = x + WO( SDPA( RoPE(Q), RoPE(K), V ) )
///   y = y + WDown( GELU(WGate(rms(y))) * WUp(rms(y)) )
///
/// Both RMSNorms use elementwise weight = 1.0 in the synthetic-weights
/// configuration (we don't randomise norm scale; it would only multiply both
/// backends by the same constant and add no signal).
pub struct CandleLayer {
    h: HParams,
    device: Device,
    wqkv: Tensor,
    wo: Tensor,
    attn_norm: Tensor,
    w_gate: Tensor,
    w_up: Tensor,
    w_down: Tensor,
    mlp_norm: Tensor,
    cos: Tensor,
    sin: Tensor,
}

impl CandleLayer {
    pub fn new(h: HParams, p: &LayerParams, device: Device) -> CandleResult<Self> {
        let dt = DType::F32;
        let h_dim = h.hidden_size;
        let i_dim = h.intermediate_size;
        let wqkv = Tensor::from_slice(&p.wqkv, (3 * h_dim, h_dim), &device)?.to_dtype(dt)?;
        let wo = Tensor::from_slice(&p.wo, (h_dim, h_dim), &device)?.to_dtype(dt)?;
        let attn_norm = Tensor::from_slice(&p.attn_norm, h_dim, &device)?.to_dtype(dt)?;
        let w_gate = Tensor::from_slice(&p.w_gate, (i_dim, h_dim), &device)?.to_dtype(dt)?;
        let w_up = Tensor::from_slice(&p.w_up, (i_dim, h_dim), &device)?.to_dtype(dt)?;
        let w_down = Tensor::from_slice(&p.w_down, (h_dim, i_dim), &device)?.to_dtype(dt)?;
        let mlp_norm = Tensor::from_slice(&p.mlp_norm, h_dim, &device)?.to_dtype(dt)?;
        let (cos, sin) = precompute_rope_cache(h, &device)?;
        Ok(Self { h, device, wqkv, wo, attn_norm, w_gate, w_up, w_down, mlp_norm, cos, sin })
    }

    /// `input`: shape `(batch, seq_len, hidden_size)`, FP32. Returns the
    /// layer output in the same shape and dtype.
    pub fn forward(&self, input: &Tensor) -> CandleResult<Tensor> {
        let h = self.h;
        let (b, s, hidden) = input.dims3()?;
        debug_assert_eq!(hidden, h.hidden_size);
        let head_dim = h.hidden_size / h.num_heads;

        // --- attention block ---
        let x_norm = rms_norm(input, &self.attn_norm, h.rms_eps)?;
        // (B, S, 3H) = (B, S, H) @ (H, 3H)
        let qkv = linear(&x_norm, &self.wqkv)?;
        // split → 3 tensors (B, S, H)
        let qkv = qkv.reshape((b, s, 3, h.num_heads, head_dim))?;
        let q = qkv.i((.., .., 0, .., ..))?.contiguous()?;
        let k = qkv.i((.., .., 1, .., ..))?.contiguous()?;
        let v = qkv.i((.., .., 2, .., ..))?.contiguous()?;
        // → (B, H, S, D)
        let q = q.transpose(1, 2)?.contiguous()?;
        let k = k.transpose(1, 2)?.contiguous()?;
        let v = v.transpose(1, 2)?.contiguous()?;
        let q = apply_rope(&q, &self.cos, &self.sin)?;
        let k = apply_rope(&k, &self.cos, &self.sin)?;
        let attn = scaled_dot_product_attention(&q, &k, &v, head_dim)?; // (B,H,S,D)
        // → (B, S, H*D)
        let attn = attn.transpose(1, 2)?.contiguous()?.reshape((b, s, h.hidden_size))?;
        let attn_out = linear(&attn, &self.wo)?;
        let y = (input + attn_out)?;

        // --- MLP block (GeGLU) ---
        let y_norm = rms_norm(&y, &self.mlp_norm, h.rms_eps)?;
        let gate = linear(&y_norm, &self.w_gate)?.gelu()?; // (B,S,I)
        let up = linear(&y_norm, &self.w_up)?; // (B,S,I)
        let geglu = (gate * up)?;
        let mlp_out = linear(&geglu, &self.w_down)?; // (B,S,H)
        let out = (y + mlp_out)?;
        Ok(out)
    }

    pub fn device(&self) -> &Device {
        &self.device
    }
}

// ----- helpers -----

fn linear(x: &Tensor, w: &Tensor) -> CandleResult<Tensor> {
    // x: (..., in), w: (out, in) → (..., out) via matmul(x, w^T).
    let w_t = w.t()?.contiguous()?;
    x.broadcast_matmul(&w_t)
}

fn rms_norm(x: &Tensor, weight: &Tensor, eps: f32) -> CandleResult<Tensor> {
    let x_f32 = x.to_dtype(DType::F32)?;
    let var = x_f32.sqr()?.mean_keepdim(D::Minus1)?;
    let denom = (var + eps as f64)?.sqrt()?;
    let n = x_f32.broadcast_div(&denom)?;
    n.broadcast_mul(weight)
}

fn precompute_rope_cache(h: HParams, device: &Device) -> CandleResult<(Tensor, Tensor)> {
    let head_dim = h.hidden_size / h.num_heads;
    let theta = h.rope_theta;
    let half = head_dim / 2;
    let inv_freq: Vec<f32> = (0..half)
        .map(|i| 1.0_f32 / theta.powf(2.0 * (i as f32) / head_dim as f32))
        .collect();
    let inv_freq = Tensor::from_vec(inv_freq, (half,), device)?;
    let t: Vec<f32> = (0..h.seq_len).map(|i| i as f32).collect();
    let t = Tensor::from_vec(t, (h.seq_len,), device)?;
    // (S, half)
    let freqs = t.unsqueeze(1)?.broadcast_mul(&inv_freq.unsqueeze(0)?)?;
    let cos = freqs.cos()?;
    let sin = freqs.sin()?;
    Ok((cos, sin))
}

fn apply_rope(x: &Tensor, cos: &Tensor, sin: &Tensor) -> CandleResult<Tensor> {
    // x: (B, H, S, D). Split last dim into (D/2, D/2) — "real, imag" halves
    // matching candle_transformers ModernBERT (which uses the non-interleaved
    // variant: rotate halves, not pairs). `RotaryPositionalEncoding` in MLX
    // with `traditional = false` (default) does the same.
    let (b, hd, s, d) = x.dims4()?;
    let half = d / 2;
    let x1 = x.narrow(D::Minus1, 0, half)?.contiguous()?;
    let x2 = x.narrow(D::Minus1, half, half)?.contiguous()?;
    // cos / sin: (S, half) → broadcast to (1, 1, S, half)
    let cos = cos.unsqueeze(0)?.unsqueeze(0)?;
    let sin = sin.unsqueeze(0)?.unsqueeze(0)?;
    let rot1 = (x1.broadcast_mul(&cos)? - x2.broadcast_mul(&sin)?)?;
    let rot2 = (x1.broadcast_mul(&sin)? + x2.broadcast_mul(&cos)?)?;
    let out = Tensor::cat(&[rot1, rot2], D::Minus1)?;
    debug_assert_eq!(out.dims(), &[b, hd, s, d]);
    Ok(out)
}

fn scaled_dot_product_attention(
    q: &Tensor,
    k: &Tensor,
    v: &Tensor,
    head_dim: usize,
) -> CandleResult<Tensor> {
    let scale = 1.0_f32 / (head_dim as f32).sqrt();
    // q,k,v: (B, H, S, D). scores: (B, H, S, S)
    let k_t = k.transpose(D::Minus2, D::Minus1)?.contiguous()?;
    let scores = q.broadcast_matmul(&k_t)?;
    let scores = (scores * (scale as f64))?;
    let probs = candle_nn::ops::softmax_last_dim(&scores)?;
    probs.broadcast_matmul(v)
}

// Convenience: pull the device-selection logic into one place so the spike
// can flip between Metal and CPU without rewiring call sites.
pub fn metal_or_cpu() -> Device {
    Device::new_metal(0).unwrap_or(Device::Cpu)
}

use candle_core::IndexOp;
