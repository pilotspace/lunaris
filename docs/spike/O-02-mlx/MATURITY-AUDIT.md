# O-02-A — `mlx-rs` maturity audit

**Date**: 2026-05-15
**Spike**: O-02 (MLX evaluation — Apple-Silicon-only alternate embed backend)
**Auditor**: senior-rust-engineer agent on behalf of v0.4 hardware-optimisation roadmap
**Scope**: decide whether `mlx-rs` (unofficial Rust wrapper for Apple MLX)
exposes enough primitives to port a single ModernBERT encoder layer, and
whether the toolchain story is acceptable for a Cargo-feature-gated
`mlx-apple` backend in `lunaris-embed-native`.

## TL;DR

| Question | Answer |
|---|---|
| Crate exists & maintained? | **Yes** — `mlx-rs 0.25.3` published 2025-12-16 (5 months ago), 20.8k 90-day downloads. |
| Required FP16-path primitives present? | **Yes** — all of `Linear`, `RmsNorm`, `LayerNorm`, RoPE (`fast::rope` + `nn::Rope`), `fast::scaled_dot_product_attention`, `softmax`, `gelu`/`silu`/`glu`, `matmul`, `safetensors` load are exposed. |
| GGUF Q4_K_M loader? | **No** (audit-eligible NO-GO **only** if quantised path were required; the spike targets the FP16 path, so this is **not blocking**). |
| Quantised matmul / Q4/Q8 ops? | **Yes** — `ops::quantization::{quantize, quantized_matmul, dequantize}` + `nn::quantized`. |
| Toolchain risk (build from source)? | **Medium** — `mlx-sys 0.2.0` uses `cmake` + `bindgen` to compile the bundled `mlx-c` C++ submodule. Host has cmake 4.3.2 + Apple clang 17 → preconditions met. Cold build cost: estimated 3-7 min one-off. |
| Recommendation | **PROCEED to O2-B** (single-layer port) on the FP16 path. Defer GGUF/quantised path to a follow-up audit if the FP16 spike returns GO. |

## 1. Crate metadata (crates.io, 2026-05-15)

Source: `https://crates.io/api/v1/crates/mlx-rs` (HTTP fetch).

| Field | Value |
|---|---|
| Name | `mlx-rs` |
| Latest stable | `0.25.3` |
| Published | 2025-12-16T06:13:49Z |
| First release | 2024-07-13 (16 months ago — v0.14.0) |
| All-time downloads | 29,648 |
| Recent (90d) downloads | 20,803 |
| Repository | `https://github.com/oxideai/mlx-rs` (unofficial; not Apple-owned) |
| Documentation | `https://oxideai.github.io/mlx-rs/mlx_rs/` |
| License | MIT OR Apache-2.0 |
| Edition | 2021 |
| MSRV declared | None (workspace MSRV 1.94 should be fine — no edition-2024 deps in mlx-rs itself) |
| Features | `default = ["accelerate", "metal"]`, `safetensors` opt-in |

**Maintenance signal**: 12 published versions across 16 months; the `0.21.x → 0.25.x` cadence (Jan → Dec 2025) shows active development. The crate is **unofficial** — Apple itself does not ship a Rust wrapper; this is a community project by `oxideai/David Chavez & Minghua Wu`. That's an adoption risk we accept for a spike but call out for production.

## 2. Primitive availability vs ModernBERT requirements

ModernBERT (granite-r2 hyperparameters from `crates/lunaris-embed-native/src/config.rs`) needs the following per-layer:

| Primitive | Required? | mlx-rs path | Status |
|---|---|---|---|
| FP16 dtype | hard requirement | `dtype::Dtype::Float16` (via `half::f16`) | **Present** |
| safetensors load | hard requirement | `Array::load_safetensors` (`src/ops/io.rs:51`) — returns `HashMap<String, Array>` | **Present** |
| GGUF Q4_K_M load | optional (quantised path only) | absent | **Missing** (acceptable — FP16 path is the spike target) |
| Linear (Q,K,V,O,gate,up,down) | hard requirement | `nn::Linear` (`src/nn/linear.rs:62`) | **Present** |
| Matmul (fused) | hard requirement | `ops::arithmetic::matmul_device` (`src/ops/arithmetic.rs:1066`) | **Present** |
| Rotary positional embedding (RoPE) | hard requirement (alternating local/global θ) | `fast::rope` / `fast::rope_device` (`src/fast.rs:15`) + `nn::Rope` builder (`src/nn/positional_encoding.rs`) | **Present** — fused fast-kernel + module-style wrapper |
| RMSNorm | hard requirement | `nn::RmsNorm` (`src/nn/normalization.rs:250`) + `fast::rms_norm` (fused) | **Present** |
| LayerNorm (final cls-pool path) | conditional (granite-r2 final norm is `LayerNorm`, not RMS) | `nn::LayerNorm` (`src/nn/normalization.rs:169`) + `fast::layer_norm` | **Present** |
| Scaled dot-product attention | hard requirement | `fast::scaled_dot_product_attention` (`src/fast.rs:117`) — fused Q·Kᵀ/√d + softmax + ·V | **Present (fused)** |
| Softmax | hard requirement (fallback if SDPA path declined) | `ops::arithmetic::softmax_device` (`src/ops/arithmetic.rs:1284`) | **Present** |
| GeGLU MLP activation | hard requirement (ModernBERT uses gated GLU with GELU) | `nn::activation::glu` + `gelu`/`gelu_approximate`/`gelu_fast_approximate` (`src/nn/activation.rs:164,194`) | **Present** — also `silu` if SwiGLU variant is added later |
| Quantised matmul (Q4 / Q8) | optional (quantised path only) | `ops::quantization::{quantize, quantized_matmul, dequantize}` + `nn::quantized` (`src/nn/quantized.rs`) | **Present** — but feeds raw quantised tensors, not GGUF blocks; would need a transcoder |
| Metal device | hard requirement | `Stream::gpu` / default-device routing; `metal` is in the default feature set | **Present** |
| Accelerate (CPU fallback) | nice-to-have | `accelerate` feature (default-on) | **Present** |

**Verdict**: all FP16-path primitives are exposed. No primitive is missing for the single-layer spike. The two "missing" items (GGUF load + GGUF-shaped quantised matmul) only matter for the future quantised path; the FP16 spike does not depend on them.

## 3. Build / toolchain risk

`mlx-rs 0.25.3` depends on `mlx-sys = 0.2.0` (pinned `=0.2.0`). `mlx-sys` is the FFI sys-crate that bundles Apple's MLX C++ runtime via the `mlx-c` shim.

Key facts from `mlx-sys-0.2.0/build.rs` + `src/mlx-c/`:

- **Build system**: `cmake` is invoked at compile time (`cmake = ?` crate dep + the host needs `cmake` on PATH).
- **C++ compiler**: needed (the bundled `src/mlx-c/mlx/` is Apple's MLX C++ source — Metal kernels are compiled there).
- **Bundled source**: `src/mlx-c/` is present in the published `.crate` (not pulled at build time via git) — no submodule init step required by downstream consumers. Good for hermeticity.
- **Cold compile cost**: 3-7 minutes the first time per target/profile (CMake + Apple Metal shader build). Cached in `target/` afterwards.
- **Host preconditions verified on this audit machine** (macOS 24.6 / arm64):
  - `cmake --version` → 4.3.2 ✓
  - `clang++ --version` → Apple clang 17.0.0 ✓
- **Risk**: any CI host that does not have `cmake` installed will fail. Document this as a prerequisite if we ever promote MLX to feature-gated production.

## 4. Decision

**O2-A verdict: PROCEED to O2-B (single-layer port).**

The crate is mature enough to bear a 100-150 LOC spike. All required FP16-path primitives are present. The unofficial-wrapper status and the cmake build precondition are real adoption costs but not blockers for the spike. GGUF is the only requirement we cannot satisfy today; that is deferred to a follow-up audit if the FP16 spike returns GO.

## 5. Open questions surfaced for DECISION.md

- Does `Array::load_safetensors` preserve the FP16 dtype byte-exact, or does it upcast to F32 on load? (Will measure in O2-C; if upcast, the drift gate sees an artificial FP32 reference and we must either downcast post-load or change the comparison reference.)
- Does `fast::scaled_dot_product_attention` accept a custom additive bias mask (for ModernBERT's local-window sliding mask)? If not, the local-attention layers will need a hand-rolled fallback path; the spike will use a global-attention layer (no local mask) to sidestep this.
- Is `Stream::gpu` work synchronous, or do we need an explicit array materialisation call before reading timings? (Criterion harness in O2-D will force materialisation on the output before stopping the timer.)

## 6. References

- `mlx-rs 0.25.3` source extracted to `/tmp/mlx-rs-src/mlx-rs-0.25.3/`
- `mlx-sys 0.2.0` source extracted to `/tmp/mlx-sys-src/mlx-sys-0.2.0/`
- `crates/lunaris-embed-native/src/config.rs` — granite-r2 hyperparameters mirrored by the spike
- `crates/lunaris-embed-native/src/modernbert.rs` — production CLS-pool + L2-normalize math (delegates layer math to `candle_transformers::ModernBert`)
