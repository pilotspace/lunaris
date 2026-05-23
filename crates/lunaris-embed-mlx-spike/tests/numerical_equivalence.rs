//! O-02-C — numerical-equivalence test (candle-Metal vs mlx-rs).
//!
//! For 50 deterministically-seeded random input panels we run both layer
//! implementations against the **same** synthetic weights and compute the
//! mean row-wise cosine similarity between the two outputs (flattened to
//! `(batch * seq_len, hidden_size)`). The spike contract sets a 0.5% mean
//! cosine-drift gate against an FP16 candle-Metal reference on a real
//! 100-prompt panel; at the kernel level with identical FP32 weights we
//! expect drift in the 1e-4..1e-3 range (float-rounding noise from kernel
//! ordering). A drift anywhere near 0.5% would be a port bug.
//!
//! Gate: mean cosine **≥ 0.995** (drift ≤ 0.5%, matching the N-01 panel
//! contract — kept identical so DECISION.md inherits N-01 wording).

use lunaris_embed_mlx_spike::layer_candle::{CandleLayer, metal_or_cpu};
use lunaris_embed_mlx_spike::layer_mlx::MlxLayer;
use lunaris_embed_mlx_spike::params::{LayerParams, granite_r2_hparams, synthetic_input};

#[test]
fn candle_vs_mlx_single_layer_mean_cosine() {
    let h = granite_r2_hparams();
    let params = LayerParams::synthetic(h, 0xC0FFEE_BAD_F00D);
    let n_panels = 50;

    let device = metal_or_cpu();
    let candle_layer = CandleLayer::new(h, &params, device.clone()).expect("candle layer");
    let mlx_layer = MlxLayer::new(h, &params).expect("mlx layer");

    let mut cos_acc = 0.0_f64;
    let mut compared_rows: usize = 0;
    let mut max_drift = 0.0_f64; // max(1.0 - cos) across all rows

    for panel_idx in 0..n_panels {
        let seed = 0xDEAD_BEEF_42_u64 ^ (panel_idx as u64);
        let input = synthetic_input(h, seed);

        // candle forward + readback
        let candle_in =
            candle_core::Tensor::from_slice(&input, (h.batch, h.seq_len, h.hidden_size), &device)
                .expect("candle input");
        let candle_out = candle_layer.forward(&candle_in).expect("candle fwd");
        let candle_vec: Vec<Vec<Vec<f32>>> = candle_out.to_vec3::<f32>().expect("candle vec");

        // mlx forward + readback
        let mlx_in = mlx_rs::Array::from_slice(
            &input,
            &[h.batch as i32, h.seq_len as i32, h.hidden_size as i32],
        );
        let mlx_out = mlx_layer.forward(&mlx_in).expect("mlx fwd");
        let mlx_vec = MlxLayer::materialise(&mlx_out).expect("mlx materialise");
        assert_eq!(mlx_vec.len(), h.batch * h.seq_len * h.hidden_size);

        // Compare row by row (rows = batch * seq_len; row dim = hidden_size).
        for b in 0..h.batch {
            for s in 0..h.seq_len {
                let candle_row = &candle_vec[b][s];
                let base = (b * h.seq_len + s) * h.hidden_size;
                let mlx_row = &mlx_vec[base..base + h.hidden_size];
                let cos = cosine_f64(candle_row, mlx_row);
                cos_acc += cos;
                let drift = 1.0_f64 - cos;
                if drift > max_drift {
                    max_drift = drift;
                }
                compared_rows += 1;
            }
        }
    }

    let mean_cos = cos_acc / compared_rows as f64;
    let mean_drift = 1.0_f64 - mean_cos;
    let mean_drift_pct = mean_drift * 100.0;
    eprintln!(
        "O2-C: panels={n_panels} rows={compared_rows} \
         mean_cos={mean_cos:.9} mean_drift={mean_drift:.3e} \
         max_drift={max_drift:.3e} drift_pct={mean_drift_pct:.6}%"
    );

    // Gate: mean cosine ≥ 0.995 (≤ 0.5% drift), inherited from N-01.
    assert!(
        mean_cos >= 0.995,
        "drift gate violated: mean_cos={mean_cos:.6} (drift {mean_drift_pct:.4}%, max_drift {max_drift:.3e}) — \
         port math diverges from candle reference. NO-GO."
    );
}

/// Row-wise cosine similarity, FP64 throughout (returned as f64 — do **not**
/// cast back to f32, that would pin values within 2^-23 of 1.0 to exactly 1.0
/// and hide real drift below ~1e-7).
fn cosine_f64(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len(), "cosine: shape mismatch");
    let mut dot = 0.0_f64;
    let mut na = 0.0_f64;
    let mut nb = 0.0_f64;
    for (&x, &y) in a.iter().zip(b.iter()) {
        let xf = x as f64;
        let yf = y as f64;
        dot += xf * yf;
        na += xf * xf;
        nb += yf * yf;
    }
    let denom = (na.sqrt() * nb.sqrt()).max(1e-12);
    dot / denom
}
