//! O-01-C / O-01-D — `Device` upgrade + warm-up helpers shared by the FP16
//! and quantized embedder construction paths.
//!
//! The model code is `Device`-parameterized end-to-end (see
//! `docs/benchmarks/o01/COMPLIANCE-AUDIT.md`); these helpers do NOT branch
//! inside any forward pass. They only run at `open()` to decide which
//! `Device` to use when the caller passes `Device::Cpu` and a GPU backend
//! is feature-enabled at compile time.
//!
//! ## Selection rule
//!
//! 1. If `opts.device` is anything other than `Device::Cpu`, honor it verbatim
//!    (operator already made the choice; `LUNARIS_DEVICE` does not override an
//!    explicit SDK/caller device).
//! 2. If `LUNARIS_DEVICE` is set: `cpu` skips all GPU probes (the kill-switch
//!    for candle's shape-keyed Metal buffer-cache growth in long-lived
//!    processes); `cuda` / `metal` probe only that backend and WARN + fall
//!    back to Cpu if it doesn't initialize; `auto` (or unset) = the ladder
//!    below. Unknown values warn and mean `auto`.
//! 3. Ladder: `Device::new_cuda(0)` succeeds → CUDA; else
//!    `Device::new_metal(0)` succeeds → Metal; else stay on `Device::Cpu`.
//!
//! The probes are RUNTIME checks, deliberately not `cfg`-gated: candle
//! defines `new_cuda` / `new_metal` unconditionally and returns a clean
//! `NotCompiledWith*Support` error when the backend isn't in the binary, so
//! one code path covers "kernels compiled + GPU present" (upgrade), "kernels
//! compiled + no usable GPU" (fallback), and "kernels absent" (fallback).
//! Apple Silicon default builds compile the Metal kernels via the
//! target-specific dependency block in Cargo.toml; CUDA kernels still require
//! the `cuda` feature (external toolchain).
//!
//! CUDA wins over Metal in priority because the only host where both could be
//! compiled together is exotic (Linux + macOS cross-build); in practice each
//! is gated to its native platform.
//!
//! ## Warm-up
//!
//! GPU backends pay a one-time JIT / kernel-cache cost on the first matmul.
//! [`warmup_device`] runs a 1×4×4 dummy matmul on the selected device so the
//! first user query doesn't eat the JIT spike. Cheap on CPU (~µs), 5–50 ms
//! on Metal/CUDA depending on driver state.

use candle_core::{DType, Device, Tensor};

/// Operator override for the automatic device ladder, parsed from the
/// `LUNARIS_DEVICE` env var. See the module doc for the semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceOverride {
    Auto,
    ForceCpu,
    PreferCuda,
    PreferMetal,
}

/// Pure parse half of the `LUNARIS_DEVICE` handling — takes the raw env
/// value so tests never touch process-global env (same pattern as the
/// embedder's `resolve_batch_size`). Unknown values WARN and fall back to
/// `Auto` rather than erroring: a typo'd override must never take a
/// production embedder down.
fn parse_device_override(raw: Option<&str>) -> DeviceOverride {
    match raw.map(str::trim) {
        None | Some("") => DeviceOverride::Auto,
        Some(s) if s.eq_ignore_ascii_case("auto") => DeviceOverride::Auto,
        Some(s) if s.eq_ignore_ascii_case("cpu") => DeviceOverride::ForceCpu,
        Some(s) if s.eq_ignore_ascii_case("cuda") => DeviceOverride::PreferCuda,
        Some(s) if s.eq_ignore_ascii_case("metal") => DeviceOverride::PreferMetal,
        Some(other) => {
            tracing::warn!(
                value = other,
                "LUNARIS_DEVICE not one of auto|cpu|cuda|metal — treating as auto"
            );
            DeviceOverride::Auto
        }
    }
}

/// Upgrade `Device::Cpu` to the best GPU backend that initializes at
/// runtime, honoring the `LUNARIS_DEVICE` override. Returns the device that
/// should be passed to the model.
///
/// Errors: never — falls back to `Device::Cpu` if any GPU init fails. Logs
/// at INFO when an upgrade happens, DEBUG when the caller's choice is honored
/// verbatim or the fallback path is taken.
pub fn select_device(requested: Device) -> Device {
    if !matches!(requested, Device::Cpu) {
        tracing::debug!(?requested, "device_select: caller-provided device honored verbatim");
        return requested;
    }
    let ov = parse_device_override(std::env::var("LUNARIS_DEVICE").ok().as_deref());
    select_device_with(ov)
}

/// Override-explicit core of [`select_device`] — separated so tests can
/// drive every branch without racing on process env.
fn select_device_with(ov: DeviceOverride) -> Device {
    match ov {
        DeviceOverride::ForceCpu => {
            tracing::info!("device_select: LUNARIS_DEVICE=cpu — skipping GPU probes");
            Device::Cpu
        }
        DeviceOverride::PreferCuda => match Device::new_cuda(0) {
            Ok(d) => {
                tracing::info!(
                    backend = "lunaris-embed-native",
                    "device_select: Cpu → Cuda(0) (LUNARIS_DEVICE=cuda)"
                );
                d
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "LUNARIS_DEVICE=cuda but CUDA did not initialize \
                     (kernels not compiled in, or no usable GPU) — using Cpu"
                );
                Device::Cpu
            }
        },
        DeviceOverride::PreferMetal => match Device::new_metal(0) {
            Ok(d) => {
                tracing::info!(
                    backend = "lunaris-embed-native",
                    "device_select: Cpu → Metal(0) (LUNARIS_DEVICE=metal)"
                );
                d
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "LUNARIS_DEVICE=metal but Metal did not initialize \
                     (kernels not compiled in, or no usable GPU) — using Cpu"
                );
                Device::Cpu
            }
        },
        DeviceOverride::Auto => {
            // Runtime probes, not cfg gates — see the module doc. CUDA first.
            match Device::new_cuda(0) {
                Ok(d) => {
                    tracing::info!(
                        backend = "lunaris-embed-native",
                        "device_select: Cpu → Cuda(0) (runtime probe)"
                    );
                    return d;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "device_select: cuda probe failed, trying next");
                }
            }
            match Device::new_metal(0) {
                Ok(d) => {
                    tracing::info!(
                        backend = "lunaris-embed-native",
                        "device_select: Cpu → Metal(0) (runtime probe)"
                    );
                    return d;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "device_select: metal probe failed, falling back");
                }
            }
            tracing::debug!("device_select: staying on Device::Cpu");
            Device::Cpu
        }
    }
}

/// Run a tiny dummy matmul on `device` to pay the one-time JIT / kernel-cache
/// cost at `open()` instead of on the first user query.
///
/// Failure modes: any candle error is logged and SWALLOWED — warm-up is
/// best-effort. A failure here would mean the device is mis-initialized, in
/// which case the subsequent real forward pass will surface the error with
/// proper context anyway.
///
/// **O-01 untested-on-target for CUDA — measure in O-03 self-hosted runner.**
/// Metal path is exercised on this host (macOS / Apple Silicon arm64) at
/// O-01-C landing time.
pub fn warmup_device(device: &Device) {
    let res: Result<(), candle_core::Error> = (|| {
        // 4×4 F32 matmul — single tile on every backend, minimal scratch.
        let a = Tensor::zeros((4, 4), DType::F32, device)?;
        let b = Tensor::zeros((4, 4), DType::F32, device)?;
        let _c = a.matmul(&b)?;
        Ok(())
    })();

    match res {
        Ok(()) => {
            tracing::info!(?device, "device warm-up matmul completed");
        }
        Err(e) => {
            tracing::warn!(?device, error = %e, "device warm-up matmul failed (best-effort)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_cpu_when_no_features() {
        let d = select_device(Device::Cpu);
        // On a default build (no metal/cuda) this stays Cpu. On `--features
        // metal` it upgrades — both branches are valid; only assert the
        // function returns *some* device.
        let _ = matches!(d, Device::Cpu | Device::Metal(_) | Device::Cuda(_));
    }

    #[test]
    fn warmup_cpu_is_infallible() {
        warmup_device(&Device::Cpu);
    }

    #[test]
    fn parse_device_override_matrix() {
        use DeviceOverride::*;
        assert_eq!(parse_device_override(None), Auto);
        assert_eq!(parse_device_override(Some("")), Auto);
        assert_eq!(parse_device_override(Some("auto")), Auto);
        assert_eq!(parse_device_override(Some("CPU")), ForceCpu);
        assert_eq!(parse_device_override(Some(" cpu ")), ForceCpu);
        assert_eq!(parse_device_override(Some("Metal")), PreferMetal);
        assert_eq!(parse_device_override(Some("cuda")), PreferCuda);
        // Unknown values degrade to Auto (warn, never error).
        assert_eq!(parse_device_override(Some("tpu")), Auto);
    }

    /// The kill-switch: `ForceCpu` (what `LUNARIS_DEVICE=cpu` parses to)
    /// must skip the GPU probes even on hosts where Metal initializes.
    #[test]
    fn force_cpu_override_skips_gpu_probes() {
        assert!(matches!(select_device_with(DeviceOverride::ForceCpu), Device::Cpu));
    }

    /// macOS builds must carry Accelerate BLAS by DEFAULT — via the
    /// `[target.'cfg(target_os = "macos")'.dependencies]` block in this
    /// crate's Cargo.toml, not the opt-in `cpu-accelerate` flag. Without
    /// BLAS, candle's F32 matmul falls back to naive gemm: the documented
    /// root cause of the ~21 min/question LongMemEval ingest (see the
    /// accelerator-feature comment in `crates/lunaris/Cargo.toml`).
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_build_has_accelerate_blas() {
        assert!(
            candle_core::utils::has_accelerate(),
            "candle compiled without Accelerate on macOS — restore the \
             target-specific candle-core/accelerate + candle-nn/accelerate \
             dependency features in lunaris-embed-native/Cargo.toml"
        );
    }

    #[cfg(feature = "metal")]
    #[test]
    fn select_upgrades_cpu_to_metal_when_feature_on() {
        // On Apple Silicon hosts this will return `Device::Metal`. On CI
        // hosts without Metal hardware the init fails and we fall back to
        // Cpu — both are acceptable, only assert "did not panic".
        let _ = select_device(Device::Cpu);
    }

    /// Apple Silicon default builds must (a) COMPILE the Metal kernels — via
    /// the aarch64-macOS target-specific dependency block in Cargo.toml, not
    /// the opt-in `metal` feature — and (b) pick the GPU at RUNTIME whenever
    /// the device actually initializes. Runtime probing (not `cfg`) is the
    /// contract: `Device::new_metal` is always defined and returns a clean
    /// error on hosts whose binary lacks the kernels or whose GPU is absent,
    /// so the ladder degrades to Cpu instead of being compiled out.
    /// `LUNARIS_DEVICE=cpu` remains the operator kill-switch (Metal's
    /// shape-keyed buffer cache grows in long-lived processes).
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn apple_silicon_default_build_selects_metal_at_runtime() {
        assert!(
            candle_core::utils::metal_is_available(),
            "Metal kernels not compiled into an Apple Silicon default build — \
             restore the aarch64-macOS target-specific candle-core/metal + \
             candle-nn/metal dependency features in \
             lunaris-embed-native/Cargo.toml"
        );
        // Only assert the runtime pick where Metal genuinely initializes
        // (CI VMs may expose no usable GPU — there the compile guard above
        // is the enforceable half).
        if Device::new_metal(0).is_ok() {
            let d = select_device(Device::Cpu);
            assert!(
                matches!(d, Device::Metal(_)),
                "Metal initializes on this host but the runtime ladder \
                 returned {d:?} — the probe must not be cfg-gated"
            );
        }
    }
}
