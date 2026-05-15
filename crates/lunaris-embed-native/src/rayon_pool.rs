//! O-01-B — one-shot rayon thread-pool init for the candle CPU backend.
//!
//! `rayon`'s global thread pool defaults to `num_cpus::get()` (logical cores
//! including hyperthreads). For BLAS-heavy matmul kernels the cache thrash
//! from over-subscription costs 1.3–1.8× without a perf win — see
//! HARDWARE-OPTIMIZATION-ROADMAP §51 ("rayon thread pool sized to physical
//! cores, not logical").
//!
//! Call [`ensure_physical_core_pool`] from `NativeEmbedder::open()` (and any
//! other CPU-bound construction site) BEFORE any candle CPU op fires. It is
//! idempotent — first call installs the pool, subsequent calls (or
//! cross-thread races) become no-ops. Rayon's `build_global` returns an
//! `Err` after the pool is already set; we treat that as success.
//!
//! Has no effect on `Device::Metal` / `Device::Cuda`. Candle's GPU backends
//! schedule their own kernels; rayon is only used for the CPU path
//! (quantized GGUF + safetensors decode + fallback matmul).

use std::sync::OnceLock;

static INIT: OnceLock<()> = OnceLock::new();

/// Install rayon's global thread pool sized to **physical** core count.
/// Idempotent: first call wins, races and re-entries return immediately.
///
/// Failure modes:
///   - `num_cpus::get_physical()` returns 0 → fall back to `1` (single-
///     threaded). This is theoretical; real hosts always report ≥1.
///   - `ThreadPoolBuilder::build_global` returns `Err` because the pool was
///     already initialized by an upstream caller → treated as success
///     (whatever pool exists is the operator's contract).
pub fn ensure_physical_core_pool() {
    INIT.get_or_init(|| {
        let phys = num_cpus::get_physical().max(1);
        match rayon::ThreadPoolBuilder::new().num_threads(phys).build_global() {
            Ok(()) => {
                tracing::info!(
                    backend = "lunaris-embed-native",
                    physical_cores = phys,
                    "rayon global pool installed at physical-core count"
                );
            }
            Err(e) => {
                // Pre-existing pool — accept the operator's choice.
                tracing::debug!(
                    backend = "lunaris-embed-native",
                    error = %e,
                    "rayon global pool already initialized; leaving as-is"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_is_idempotent() {
        // Two calls must not panic — second is a no-op via `OnceLock`.
        ensure_physical_core_pool();
        ensure_physical_core_pool();
    }
}
