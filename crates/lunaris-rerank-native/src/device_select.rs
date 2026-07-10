//! O-01-C / O-01-D — `Device` upgrade + warm-up helpers shared by the FP32
//! and quantized reranker construction paths. Mirror of
//! `lunaris-embed-native::device_select` — see that module for the full
//! rationale.

use candle_core::{DType, Device, Tensor};

pub fn select_device(requested: Device) -> Device {
    if !matches!(requested, Device::Cpu) {
        tracing::debug!(?requested, "device_select: caller-provided device honored verbatim");
        return requested;
    }

    #[cfg(feature = "cuda")]
    {
        match Device::new_cuda(0) {
            Ok(d) => {
                tracing::info!(
                    backend = "lunaris-rerank-native",
                    "device_select: Cpu → Cuda(0) (cuda feature on)"
                );
                return d;
            }
            Err(e) => {
                tracing::debug!(error = %e, "device_select: cuda init failed");
            }
        }
    }

    #[cfg(feature = "metal")]
    {
        match Device::new_metal(0) {
            Ok(d) => {
                tracing::info!(
                    backend = "lunaris-rerank-native",
                    "device_select: Cpu → Metal(0) (metal feature on)"
                );
                return d;
            }
            Err(e) => {
                tracing::debug!(error = %e, "device_select: metal init failed");
            }
        }
    }

    Device::Cpu
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
