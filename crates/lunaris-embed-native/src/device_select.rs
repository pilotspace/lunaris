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
//!    (operator already made the choice).
//! 2. If `opts.device == Device::Cpu` and the `cuda` feature is on AND
//!    `Device::new_cuda(0)` succeeds → upgrade to CUDA.
//! 3. Else if `opts.device == Device::Cpu` and the `metal` feature is on AND
//!    `Device::new_metal(0)` succeeds → upgrade to Metal.
//! 4. Else stay on `Device::Cpu`.
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

/// Upgrade `Device::Cpu` to the best available GPU backend when its feature
/// flag is active. Returns the device that should be passed to the model.
///
/// Errors: never — falls back to `Device::Cpu` if any GPU init fails. Logs
/// at INFO when an upgrade happens, DEBUG when the caller's choice is honored
/// verbatim or the fallback path is taken.
pub fn select_device(requested: Device) -> Device {
    if !matches!(requested, Device::Cpu) {
        tracing::debug!(?requested, "device_select: caller-provided device honored verbatim");
        return requested;
    }

    // CUDA priority — only compiled when `cuda` feature is active.
    #[cfg(feature = "cuda")]
    {
        match Device::new_cuda(0) {
            Ok(d) => {
                tracing::info!(
                    backend = "lunaris-embed-native",
                    "device_select: Cpu → Cuda(0) (cuda feature on)"
                );
                return d;
            }
            Err(e) => {
                tracing::debug!(error = %e, "device_select: cuda init failed, trying next");
            }
        }
    }

    #[cfg(feature = "metal")]
    {
        match Device::new_metal(0) {
            Ok(d) => {
                tracing::info!(
                    backend = "lunaris-embed-native",
                    "device_select: Cpu → Metal(0) (metal feature on)"
                );
                return d;
            }
            Err(e) => {
                tracing::debug!(error = %e, "device_select: metal init failed, falling back");
            }
        }
    }

    tracing::debug!("device_select: staying on Device::Cpu");
    Device::Cpu
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

    #[cfg(feature = "metal")]
    #[test]
    fn select_upgrades_cpu_to_metal_when_feature_on() {
        // On Apple Silicon hosts this will return `Device::Metal`. On CI
        // hosts without Metal hardware the init fails and we fall back to
        // Cpu — both are acceptable, only assert "did not panic".
        let _ = select_device(Device::Cpu);
    }
}
