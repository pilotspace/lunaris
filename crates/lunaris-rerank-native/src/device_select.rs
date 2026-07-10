//! O-01-C / O-01-D — `Device` upgrade + warm-up helpers shared by the FP32
//! and quantized reranker construction paths. Mirror of
//! `lunaris-embed-native::device_select` — see that module for the full
//! rationale.

use candle_core::{DType, Device, Tensor};

/// Operator override for the automatic device ladder (`LUNARIS_DEVICE`) —
/// mirror of `lunaris-embed-native::device_select::DeviceOverride`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceOverride {
    Auto,
    ForceCpu,
    PreferCuda,
    PreferMetal,
}

/// Pure parse half of the `LUNARIS_DEVICE` handling — mirror of
/// `lunaris-embed-native::device_select::parse_device_override`.
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

pub fn select_device(requested: Device) -> Device {
    if !matches!(requested, Device::Cpu) {
        tracing::debug!(?requested, "device_select: caller-provided device honored verbatim");
        return requested;
    }
    let ov = parse_device_override(std::env::var("LUNARIS_DEVICE").ok().as_deref());
    select_device_with(ov)
}

/// Override-explicit core of [`select_device`] — runtime probes, not cfg
/// gates (see the embed-native module doc for the full rationale).
fn select_device_with(ov: DeviceOverride) -> Device {
    match ov {
        DeviceOverride::ForceCpu => {
            tracing::info!("device_select: LUNARIS_DEVICE=cpu — skipping GPU probes");
            Device::Cpu
        }
        DeviceOverride::PreferCuda => match Device::new_cuda(0) {
            Ok(d) => {
                tracing::info!(
                    backend = "lunaris-rerank-native",
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
                    backend = "lunaris-rerank-native",
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
            match Device::new_cuda(0) {
                Ok(d) => {
                    tracing::info!(
                        backend = "lunaris-rerank-native",
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
                        backend = "lunaris-rerank-native",
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

/// Tiny dummy matmul to pay GPU JIT cost up front. Best-effort; errors are
/// swallowed (real forward pass will surface them with context).
///
/// **O-01 untested-on-target for CUDA — measure in O-03 self-hosted runner.**
pub fn warmup_device(device: &Device) {
    let res: Result<(), candle_core::Error> = (|| {
        let a = Tensor::zeros((4, 4), DType::F32, device)?;
        let b = Tensor::zeros((4, 4), DType::F32, device)?;
        let _c = a.matmul(&b)?;
        Ok(())
    })();

    if let Err(e) = res {
        tracing::warn!(?device, error = %e, "device warm-up matmul failed (best-effort)");
    } else {
        tracing::info!(?device, "device warm-up matmul completed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmup_cpu_is_infallible() {
        warmup_device(&Device::Cpu);
    }

    #[test]
    fn select_does_not_panic() {
        let _ = select_device(Device::Cpu);
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

    /// Mirror of `lunaris-embed-native::device_select`'s Apple-Silicon
    /// runtime-Metal contract — see that test for the full rationale.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn apple_silicon_default_build_selects_metal_at_runtime() {
        assert!(
            candle_core::utils::metal_is_available(),
            "Metal kernels not compiled into an Apple Silicon default build — \
             restore the aarch64-macOS target-specific candle-core/metal + \
             candle-nn/metal dependency features in \
             lunaris-rerank-native/Cargo.toml"
        );
        if Device::new_metal(0).is_ok() {
            let d = select_device(Device::Cpu);
            assert!(
                matches!(d, Device::Metal(_)),
                "Metal initializes on this host but the runtime ladder \
                 returned {d:?} — the probe must not be cfg-gated"
            );
        }
    }

    /// Mirror of `lunaris-embed-native::device_select`'s macOS-BLAS default
    /// contract — see that test for the full rationale.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_build_has_accelerate_blas() {
        assert!(
            candle_core::utils::has_accelerate(),
            "candle compiled without Accelerate on macOS — restore the \
             target-specific candle-core/accelerate + candle-nn/accelerate \
             dependency features in lunaris-rerank-native/Cargo.toml"
        );
    }
}
