//! O-01-D — CUDA backend wiring notes.
//!
//! **All code in this module is `#[cfg(feature = "cuda")]`-gated and
//! UNTESTED on the O-01 author's host (macOS / Apple Silicon arm64). The
//! O-03 self-hosted CUDA runner is the first place these paths execute.**
//!
//! ## What is wired in O-01-D
//!
//! 1. **`cuda` Cargo feature** — declared in O-01-A on both native crates,
//!    activates `candle-core/{cuda,cudnn}` + `candle-nn/cuda`. cuDNN is
//!    pulled unconditionally (not a sub-feature) because every sm_70+
//!    GPU has cuDNN available; the cost of pulling it is negligible vs
//!    losing the 1.5–2× attention kernel speed-up.
//!
//! 2. **`Device::new_cuda(0)` upgrade** — `device_select::select_device`
//!    (O-01-C) tries CUDA before Metal when both features are on. In
//!    practice each is gated to its native platform; the priority order
//!    matters only on exotic cross-build configurations.
//!
//! 3. **Warm-up matmul** — `device_select::warmup_device` runs the same
//!    4×4 F32 matmul on CUDA as on every other backend. Pays the cuDNN
//!    handle init + first cuBLAS algorithm-selection cost up front.
//!
//! ## What is gated separately
//!
//! ### `cuda-fa2` — FlashAttention 2 kernels
//!
//! Activates `candle-transformers/flash-attn`, which routes ModernBERT's
//! attention through FA2 instead of the standard candle attention path.
//!
//! **Known risk** (per HARDWARE-OPTIMIZATION-ROADMAP risk register row 2):
//! FA2 may not compose with ModernBERT's alternating local/global attention
//! windows. The window-mask shape that FA2 accepts is narrower than
//! ModernBERT's. If a forward pass panics or returns NaN under
//! `cuda-fa2`, the documented fall-back is plain `cuda` without `cuda-fa2`.
//! Revisit when upstream FA2 supports sliding-window attention.
//!
//! Resolution path is O-02 if it slips a perf gate.
//!
//! ### Pinned host memory for tokenization staging
//!
//! HARDWARE-OPTIMIZATION-ROADMAP §49 calls out "pinned host memory for
//! tokenization staging" as a CUDA gain knob. `tokenizers 0.22` does not
//! expose a `cudaMallocHost`-backed buffer option as of this writing. The
//! `candle-core` side could allocate pinned host memory and copy the
//! tokenizer output into it before the H2D transfer, but the marginal gain
//! on a 128-token batch (~512 B of `u32` ids) is below noise — the H2D
//! transfer cost is dominated by activation tensors, not inputs.
//!
//! **Deferred to O-02 / O-03** with the explicit note that the dominant
//! H2D cost is the weight upload (one-shot at `open()`, already gone by
//! steady state) and the activation tensors (intra-graph, never touch host
//! memory after `forward()` enters).
//!
//! ### CUDA graph capture for the steady-state embed loop
//!
//! HARDWARE-OPTIMIZATION-ROADMAP §49 lists "CUDA graph capture for
//! steady-state embed loop" as a target. candle 0.10's CUDA backend does
//! not expose a public graph-capture API (only the upstream cuBLAS /
//! cuDNN handles). Implementing graph capture requires either
//! `cudarc::driver::CudaStream::capture_begin / capture_end` or a candle
//! upstream PR. **Deferred — file a separate ticket if O-03 measurements
//! fail the CUDA p50 gate.**

#![cfg(feature = "cuda")]

// O-01 CUDA untested-on-target — measure in O-03 CI.
// This stub exists so the module is reachable and any future cuda-only
// helpers have a documented home. No runtime behavior.

/// Returns `true` when the binary was built with the `cuda` Cargo feature.
/// Diagnostic-only — `Lunaris::open()` should not branch on this; the
/// `Device::new_cuda(0)` attempt + fallback in `device_select` is the
/// canonical runtime decision point.
pub fn built_with_cuda() -> bool {
    true
}
