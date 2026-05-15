# O-02 — MLX evaluation spike: DECISION

**Date**: 2026-05-15
**Spike**: O-02 (MLX evaluation — Apple-Silicon-only alternate embed backend)
**Owner**: hardware-optimisation roadmap (v0.4 milestone N-04 follow-on)
**Verdict**: **GO** — port ModernBERT embedder + reranker to MLX behind a
feature-gated `mlx-apple` backend in `lunaris-embed-native` /
`lunaris-rerank-native`. Recommended landing: v0.5+.

---

## 1. GO/NO-GO summary

| Gate | Threshold | Measured | Verdict |
|---|---|---|---|
| Embed throughput ratio (candle-Metal / MLX-Metal, p50) | ≥ 1.5× | **8.74×** | **PASS** |
| Mean cosine drift vs candle-Metal reference (single layer) | ≤ 0.5% | **8.6 × 10⁻⁷ %** | **PASS** |
| `mlx-rs` primitive coverage (FP16 path) | all required present | all present | **PASS** |
| `mlx-rs` toolchain feasibility | builds on host | cmake 4.3.2 + Apple clang 17 → clean build | **PASS** |

Both GO criteria hold with ~5× safety margin on throughput and ~5 orders
of magnitude on drift. Verdict: **GO**.

## 2. Maturity findings (from O2-A)

Full audit: `docs/spike/O-02-mlx/MATURITY-AUDIT.md`.

- `mlx-rs 0.25.3` (2025-12-16) — actively maintained, 20.8k 90-day downloads,
  MIT/Apache-2.0, unofficial wrapper over Apple's MLX C++ runtime.
- All FP16-path primitives exposed: `nn::{Linear, RmsNorm, LayerNorm}`, fused
  `fast::{rope, scaled_dot_product_attention, rms_norm, layer_norm}`,
  `ops::{matmul, softmax}`, `nn::{gelu, silu, glu}`, `Array::load_safetensors`.
- Quantised path: `ops::quantization::{quantize, quantized_matmul, dequantize}`
  and `nn::quantized` present, but **no GGUF Q4_K_M loader**. The FP16 path
  does not require it; a GGUF transcoder would be needed if and when we want
  the Q4_K_M variant on MLX.
- Toolchain: `mlx-sys 0.2.0` bundles `mlx-c` and builds via cmake at compile
  time. One-off cold build ~3-7 min; host preconditions (cmake, clang++)
  are standard on macOS dev machines and on `macos-15-arm64` CI runners.

## 3. Numerical-fidelity findings (from O2-C)

- 50 deterministically-seeded random input panels × 8 batch × 128 seq =
  **51 200 row-wise cosine comparisons**.
- FP64 cosine accumulator (the v0.1 of this test cast back to f32 and
  reported "0.0000%" — a measurement artifact; the f32 cast pinned cos
  within `2^-23` of 1.0 to exactly 1.0. v0.2 keeps f64 throughout).
- **Mean drift** = 8.6 × 10⁻⁹ ≈ 8.6 × 10⁻⁷ %.
- **Max drift** = 1.5 × 10⁻⁸.

That is roughly five orders of magnitude inside the 0.5% N-01 drift
gate. The two implementations agree to better than FP32 epsilon on
identical FP32 weights.

**Important caveat for DECISION.md readers**: the N-01 0.5% drift gate
was specified against an **FP16 candle-Metal reference on a real
100-prompt multilingual panel** (granite-r2 weights, full 22-layer
forward). The spike measured drift on a **synthetic-weights single
layer in FP32 on Apple-Silicon Metal**, which is a stricter test in
some axes (kernel-vs-kernel comparison) and a looser one in others
(no FP16 rounding, no real-text tokenisation path). The
~10⁻⁷-% number proves the per-layer math is byte-equivalent to within
float rounding; the real-weights, full-stack drift number will be
measured during the full port (O2-port-A below) on the existing
N-01 100-prompt panel.

## 4. Throughput findings (from O2-D)

Run on macOS 24.6 / arm64 (M-series Apple Silicon). Both backends
verified on Metal (the bench `panic!`s if either falls back to CPU).

| Backend | p50 median | mean | mean CI (95%) |
|---|---|---|---|
| candle-Metal (0.10.2) | **26.702 ms** | 26.704 ms | [26.696, 26.712] |
| mlx-rs / MLX-Metal (0.25.3) | **3.054 ms** | 3.073 ms | [3.074, 3.138] |

**Ratio (median): candle / mlx = 26.702 / 3.054 = 8.74×**.

The win is dominated by `fast::scaled_dot_product_attention` — MLX
ships one fused Metal kernel; candle materialises QKᵀ, softmax, ·V as
three separate compute-graph nodes. RMSNorm and RoPE fusion contribute
the rest. Even if we discount the result by 2× for measurement noise
(unlikely — IQR is ±0.03ms on MLX), the 4.4× would still clear the
1.5× gate by ~3×.

Caveat: the synthetic layer is global-attention only (no sliding-window
mask) and uses one layer rather than the full 22. The full-stack ratio
on real weights will be smaller (the production embedder also pays
tokenisation + CLS-pool + L2-normalise cost, which is identical across
backends), but the per-layer kernel ratio is the gating quantity for
the "does MLX beat candle on Apple Silicon" question, and it clears.

## 5. Final verdict

**GO** — port ModernBERT to MLX behind a feature-gated `mlx-apple`
backend in `lunaris-embed-native`, mutually exclusive with `metal`.
Recommended timeline: **v0.5+**, NOT in the current 7-day v0.4
rollout — the spike took 1 calendar day; the full port is a multi-day
job that should not displace v0.4 ship-blockers.

## 6. Port spec (full ModernBERT MLX backend)

### 6.1 Scope

- Embedder: `granite-embedding-311m-multilingual-r2` (22 layers, 768d,
  alternating local/global attention, FP16).
- Reranker: `bge-reranker-v2-m3` (24 layers XLM-RoBERTa, FP32).
- Quantised path: deferred until a GGUF→MLX transcoder is implemented;
  the FP16 path is what unlocks Apple-Silicon perf wins for v0.5.

### 6.2 File structure

```
crates/lunaris-embed-native/
├── src/
│   ├── lib.rs                       # NativeEmbedder enum dispatcher
│   ├── candle.rs                    # existing candle path (unchanged)
│   ├── mlx.rs                       # NEW — MLX path; feature = "mlx-apple"
│   ├── mlx_modernbert.rs            # NEW — 22-layer port from spike
│   ├── mlx_loader.rs                # NEW — safetensors → MLX arrays
│   ├── mlx_local_mask.rs            # NEW — sliding-window mask (the spike
│   │                                # skipped this; needed for non-global layers)
│   ├── modernbert.rs                # existing pooling/normalize (unchanged)
│   └── config.rs                    # existing (unchanged)
└── Cargo.toml
    └── [features]
        mlx-apple = ["dep:mlx-rs"]  # mutually exclusive with metal at runtime
```

Same structure mirrored under `lunaris-rerank-native/` for the reranker.

### 6.3 Feature-flag rules

- `mlx-apple` and `metal` may both be in the `[features]` table but the
  runtime dispatcher (`NativeEmbedder::open`) must reject configurations
  that enable both: log a one-line ERROR and return
  `NativeEmbedderError::ConflictingBackends` so the caller can't get a
  silent split-brain (half tensors on Metal-via-candle, half on
  Metal-via-MLX). Test in `lunaris-conformance`.
- `mlx-apple` is **never** in `default-features`. It is a manual opt-in
  via `cargo add lunaris --features mlx-apple` (or the Python /
  TypeScript SDK config knob `EmbedderConfig::native().with_mlx_apple()`).
- Target gate: `mlx-apple` only compiles on
  `target_os = "macos", target_arch = "aarch64"`. Build script emits a
  `compile_error!` on any other host.

### 6.4 Drift + perf gates (inherited from N-01)

These run in CI on every push that touches `lunaris-embed-native` with
`--features mlx-apple` on a `macos-15-arm64` runner:

- **N-01 drift gate**: mean cosine drift vs FP16 candle-Metal reference
  on the existing 100-prompt panel, ≤ 0.5%. Test:
  `lunaris-conformance::tests::mlx_drift_vs_candle_metal_n01`.
- **Embed p50 ≤ 5 ms**: the existing Apple-Silicon gate in
  `crates/lunaris-bench/benches/per_device.rs`. With MLX we expect to
  beat it by ~5-8×, but the gate stays at 5ms to leave headroom for
  the tokeniser + CLS-pool + L2-normalise cost that the spike did not
  measure.

### 6.5 Open work surfaced by the spike

1. **Local-window mask** — granite-r2 alternates global / local
   (sliding-window 128) attention layers every 3rd layer. The spike
   tested global only. Full port must build a `(seq_len, seq_len)`
   additive bias tensor and pass it to `fast::sdpa(..., mask = Some(&bias))`,
   or fall back to manual `softmax(QKᵀ/√d + bias)·V` if the fused kernel
   refuses arbitrary biases (TBD; `fast.rs:117` signature accepts an
   `Option<&Array>` mask but the spike did not exercise the path).
2. **safetensors loading semantics** — open question from MATURITY-AUDIT
   §5: does `Array::load_safetensors` preserve FP16 byte-exact or
   upcast? Verify before the drift test against real weights, otherwise
   the FP16 reference and the MLX path may compare different dtypes.
3. **GGUF transcoder** — defer to a separate spike if/when we want the
   Q4_K_M path on MLX.
4. **CI runner** — `macos-15-arm64` needs cmake installed on the
   default image. Verify in the v0.5 CI bring-up; if absent, add
   `brew install cmake` to the runner setup.

### 6.6 Acceptance criteria for the full port (DoD)

- `lunaris-embed-native --features mlx-apple` compiles on
  `macos-15-arm64`, fails to compile on every other target.
- `cargo test -p lunaris-conformance --features mlx-apple
  --test n01_drift` reports mean cosine ≥ 0.995 on the 100-prompt
  panel.
- `cargo bench -p lunaris-bench --features mlx-apple --bench
  per_device` reports embed p50 ≤ 5 ms / p99 ≤ 15 ms on Apple
  Silicon — same gate the candle-Metal path holds today.
- Documentation: `docs/migration/0.4-to-0.5-mlx-apple.md` with
  feature-flag rules, the conflicting-backends error contract, and a
  10-line "how to opt in" recipe.

### 6.7 Rollout plan

1. v0.5 — land the `mlx-apple` feature; off by default; document
   opt-in. CI gates run on every push that touches the feature.
2. v0.5.x — flip `mlx-apple` to `default-features` **only** for the
   `macos-aarch64` target if the production Helios traffic at
   `lunaris.dev` shows zero drift regressions and the p50 win
   reproduces on production traffic (not just the spike panel).
3. v0.6+ — consider deprecating candle-Metal on Apple-Silicon if MLX
   is the clear winner across all production workloads. No commitment
   until the v0.5 telemetry is in.

## 7. Spike artifacts (commit SHAs)

- `cf84496` — O-02-A maturity audit
- `e87fb4c` — O-02-B single-layer ModernBERT MLX port (smoke + lib)
- `c67a6fc` — O-02-C numerical-equivalence test
- `65c45a6` — O-02-D Metal-vs-Metal throughput bench

All in worktree `o02-mlx-spike` on branch `o02-mlx-eval`, base `8cf5232`.
