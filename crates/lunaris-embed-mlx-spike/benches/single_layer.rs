//! O-02-D — single-layer throughput micro-bench (candle-Metal vs mlx-rs).
//!
//! Each backend gets 1000 timed iterations after a 25-iteration untimed
//! warm-up (Metal shader compile / kernel autotune). We force device-graph
//! materialisation before stopping the timer so a lazy compute graph cannot
//! hide compute cost in the "stop" boundary.
//!
//! Reported numbers feed `docs/spike/O-02-mlx/DECISION.md` via the criterion
//! HTML report under `target/criterion/o02_single_layer/`.

use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};

use lunaris_embed_mlx_spike::layer_candle::{CandleLayer, metal_or_cpu};
use lunaris_embed_mlx_spike::layer_mlx::MlxLayer;
use lunaris_embed_mlx_spike::params::{LayerParams, granite_r2_hparams, synthetic_input};

fn bench_layer(c: &mut Criterion) {
    let h = granite_r2_hparams();
    let params = LayerParams::synthetic(h, 0xC0FFEE_BAD_F00D);
    let input_vec = synthetic_input(h, 0xDEAD_BEEF_42);

    // -------- candle-Metal --------
    let device = metal_or_cpu();
    // Fail loud (and abort the bench) if Metal silently fell back to CPU —
    // a candle-CPU run vs MLX-Metal would be a useless head-to-head. The
    // bench is only valid when both backends sit on the same accelerator.
    eprintln!("O2-D: candle device = {device:?}");
    assert!(
        !matches!(device, candle_core::Device::Cpu),
        "candle Metal device unavailable — refusing to run head-to-head against MLX-Metal"
    );
    let candle_layer =
        CandleLayer::new(h, &params, device.clone()).expect("candle CandleLayer::new");
    let candle_input = candle_core::Tensor::from_slice(
        &input_vec,
        (h.batch, h.seq_len, h.hidden_size),
        &device,
    )
    .expect("candle input tensor");
    // Warm-up.
    for _ in 0..25 {
        let y = candle_layer.forward(&candle_input).expect("candle warmup");
        let _ = y.to_vec3::<f32>().expect("candle materialise");
    }

    let mut g = c.benchmark_group("o02_single_layer");
    g.measurement_time(Duration::from_secs(15));
    g.sample_size(50);
    g.bench_function("candle_metal", |b| {
        b.iter(|| {
            let y = candle_layer.forward(&candle_input).expect("candle fwd");
            // Force readback so the timer captures device-side compute.
            let _ = y.to_vec3::<f32>().expect("candle materialise");
        });
    });

    // -------- mlx-rs --------
    // MLX defaults to the GPU device on Apple Silicon (see
    // `mlx_rs::Device::set_default` docstring). We pin and report it explicitly
    // so the head-to-head can't silently switch substrates between runs.
    let mlx_default = mlx_rs::Device::default();
    let mlx_kind = mlx_default
        .get_type()
        .map(|t| format!("{t:?}"))
        .unwrap_or_else(|_| "unknown".into());
    eprintln!("O2-D: mlx default device = {mlx_default:?} (type={mlx_kind})");
    assert!(
        mlx_kind.contains("Gpu"),
        "MLX default device is not GPU — refusing to publish a misleading ratio"
    );

    let mlx_layer = MlxLayer::new(h, &params).expect("MlxLayer::new");
    let mlx_input = mlx_rs::Array::from_slice(
        &input_vec,
        &[h.batch as i32, h.seq_len as i32, h.hidden_size as i32],
    );
    for _ in 0..25 {
        let y = mlx_layer.forward(&mlx_input).expect("mlx warmup");
        let _ = MlxLayer::materialise(&y).expect("mlx materialise");
    }
    g.bench_function("mlx_metal", |b| {
        b.iter(|| {
            let y = mlx_layer.forward(&mlx_input).expect("mlx fwd");
            let _ = MlxLayer::materialise(&y).expect("mlx materialise");
        });
    });

    g.finish();
}

criterion_group!(o02_benches, bench_layer);
criterion_main!(o02_benches);
