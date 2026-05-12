//! ORT execution-provider plumbing for the fastembed backends (Phase 20 Plan
//! 20-01).
//!
//! Split out of `fastembed.rs` to keep that module under the project file-size
//! threshold while still living next to its only caller. The
//! [`ExecutionPreference`] enum + parser + provider-builder are also re-used by
//! [`crate::fastembed::FastembedEmbedder::from_user_defined`] and exposed via
//! `pub use` at the parent module level.
//!
//! ## Why feature-gated variants
//!
//! `CoreMlThenCpu` and `CudaThenCpu` are `#[cfg]`-gated behind their respective
//! Cargo features so a CPU-only build of `lunaris-embed` cannot construct an
//! `ExecutionPreference` value that asks for an accelerator. The `parse_*`
//! helpers handle the case where an operator sets
//! `LUNARIS_FASTEMBED_EXECUTION=cuda` against a binary built without
//! `fastembed-cuda` — they emit a `tracing::warn` and resolve to `Cpu` so the
//! process still boots.
//!
//! ## Best-effort fallback contract
//!
//! Any accelerator construction failure (toolkit absent, version mismatch,
//! framework unavailable) MUST surface as `tracing::warn!` plus a CPU retry —
//! never a panic, never a `Result::Err`. The fallback helpers in
//! [`crate::fastembed`] implement this contract; this module only exposes the
//! preference enum + the provider-list builder.

#[cfg(any(feature = "fastembed-coreml", feature = "fastembed-cuda"))]
use fastembed::ExecutionProviderDispatch;

/// Environment variable that selects the ORT execution provider preference
/// for both fastembed-backed embedders AND rerankers.
///
/// Accepted values (case-insensitive):
/// - `cpu` — default; never attempt an accelerator.
/// - `coreml` — try Apple CoreML, fall back to CPU on failure (requires
///   `fastembed-coreml`).
/// - `cuda`  — try NVIDIA CUDA, fall back to CPU on failure (requires
///   `fastembed-cuda`).
///
/// Unknown values resolve to `Cpu` with a `tracing::warn` so a typo never
/// silently downgrades observability of operator intent.
pub const FASTEMBED_EXECUTION_ENV: &str = "LUNARIS_FASTEMBED_EXECUTION";

/// Operator-facing preference for ORT execution provider. Resolved at
/// construction time inside the fastembed embedder constructors.
///
/// The accelerator variants are `#[cfg]`-gated behind their respective Cargo
/// features so a binary built `--no-default-features --features fastembed`
/// (CPU-only) compiles cleanly — the variants don't exist if the underlying
/// ORT EP isn't linked in.
///
/// **All accelerator variants are best-effort:** if the provider can't
/// initialize at session-build time (CUDA toolkit absent, CoreML framework
/// unavailable, runtime version mismatch), the construction code falls back
/// to CPU and emits a single `tracing::warn` per construction. No panics; no
/// `Result` surface change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ExecutionPreference {
    /// CPU-only — the default. `with_execution_providers` is not called.
    #[default]
    Cpu,
    /// Try Apple CoreML; on failure, retry without providers (CPU).
    #[cfg(feature = "fastembed-coreml")]
    CoreMlThenCpu,
    /// Try NVIDIA CUDA; on failure, retry without providers (CPU).
    #[cfg(feature = "fastembed-cuda")]
    CudaThenCpu,
}

/// Resolve an [`ExecutionPreference`] from a string. Unknown or unparseable
/// values emit `tracing::warn` and return `Cpu`.
///
/// This helper is intentionally `pub` so the parallel
/// `lunaris_rerank::fastembed` module can resolve the *same* env variable
/// with the *same* semantics — operators set the variable once and both
/// backends honour it.
pub fn parse_execution(s: &str) -> ExecutionPreference {
    match s.trim().to_ascii_lowercase().as_str() {
        "" | "cpu" => ExecutionPreference::Cpu,
        #[cfg(feature = "fastembed-coreml")]
        "coreml" | "coreml_then_cpu" | "coreml-then-cpu" => ExecutionPreference::CoreMlThenCpu,
        #[cfg(feature = "fastembed-cuda")]
        "cuda" | "cuda_then_cpu" | "cuda-then-cpu" => ExecutionPreference::CudaThenCpu,
        #[cfg(not(feature = "fastembed-coreml"))]
        "coreml" | "coreml_then_cpu" | "coreml-then-cpu" => {
            tracing::warn!(
                requested = %s,
                "LUNARIS_FASTEMBED_EXECUTION=coreml but lunaris-embed was built without `fastembed-coreml` — defaulting to cpu"
            );
            ExecutionPreference::Cpu
        }
        #[cfg(not(feature = "fastembed-cuda"))]
        "cuda" | "cuda_then_cpu" | "cuda-then-cpu" => {
            tracing::warn!(
                requested = %s,
                "LUNARIS_FASTEMBED_EXECUTION=cuda but lunaris-embed was built without `fastembed-cuda` — defaulting to cpu"
            );
            ExecutionPreference::Cpu
        }
        other => {
            tracing::warn!(
                requested = %other,
                "unknown LUNARIS_FASTEMBED_EXECUTION value, defaulting to cpu"
            );
            ExecutionPreference::Cpu
        }
    }
}

/// Resolve an [`ExecutionPreference`] from the process environment.
/// Convenience wrapper over [`parse_execution`].
pub fn execution_from_env() -> ExecutionPreference {
    match std::env::var(FASTEMBED_EXECUTION_ENV) {
        Ok(v) if !v.is_empty() => parse_execution(&v),
        _ => ExecutionPreference::Cpu,
    }
}

/// Build the provider-dispatch list for the requested preference. Empty for
/// `Cpu` (the caller MUST skip `with_execution_providers` entirely so
/// fastembed's default CPU path runs).
#[cfg(any(feature = "fastembed-coreml", feature = "fastembed-cuda"))]
#[allow(unused_variables)] // arms vary by feature combination
pub(crate) fn build_execution_providers(
    pref: &ExecutionPreference,
) -> Vec<ExecutionProviderDispatch> {
    match pref {
        ExecutionPreference::Cpu => Vec::new(),
        #[cfg(feature = "fastembed-coreml")]
        ExecutionPreference::CoreMlThenCpu => vec![ort::ep::CoreML::default().build()],
        #[cfg(feature = "fastembed-cuda")]
        ExecutionPreference::CudaThenCpu => vec![ort::ep::CUDA::default().build()],
    }
}

/// CPU-only feature combo: `Cpu` is the only possible variant, so the
/// provider list is always empty. Kept as a separate function so the call
/// site doesn't need its own `#[cfg]` arm.
#[cfg(not(any(feature = "fastembed-coreml", feature = "fastembed-cuda")))]
pub(crate) fn build_execution_providers(
    _pref: &ExecutionPreference,
) -> Vec<fastembed::ExecutionProviderDispatch> {
    Vec::new()
}

/// `true` if the preference asks for an accelerator (non-`Cpu`).
pub(crate) fn requests_accelerator(pref: &ExecutionPreference) -> bool {
    !matches!(pref, ExecutionPreference::Cpu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_pref_default_is_cpu() {
        assert_eq!(ExecutionPreference::default(), ExecutionPreference::Cpu);
    }

    #[test]
    fn execution_pref_unknown_env_defaults_to_cpu() {
        assert_eq!(parse_execution("nonsense"), ExecutionPreference::Cpu);
        assert_eq!(parse_execution(""), ExecutionPreference::Cpu);
        assert_eq!(parse_execution("CPU"), ExecutionPreference::Cpu);
        assert_eq!(parse_execution("cpu"), ExecutionPreference::Cpu);
    }

    #[cfg(feature = "fastembed-coreml")]
    #[test]
    fn execution_pref_coreml_parses_when_feature_on() {
        assert_eq!(parse_execution("coreml"), ExecutionPreference::CoreMlThenCpu);
        assert_eq!(parse_execution("CoreML"), ExecutionPreference::CoreMlThenCpu);
    }

    #[cfg(feature = "fastembed-cuda")]
    #[test]
    fn execution_pref_cuda_parses_when_feature_on() {
        assert_eq!(parse_execution("cuda"), ExecutionPreference::CudaThenCpu);
        assert_eq!(parse_execution("CUDA"), ExecutionPreference::CudaThenCpu);
    }

    #[test]
    fn build_execution_providers_cpu_is_empty() {
        assert!(build_execution_providers(&ExecutionPreference::Cpu).is_empty());
    }

    #[cfg(feature = "fastembed-coreml")]
    #[test]
    fn build_execution_providers_coreml_nonempty() {
        assert_eq!(build_execution_providers(&ExecutionPreference::CoreMlThenCpu).len(), 1);
    }

    #[cfg(feature = "fastembed-cuda")]
    #[test]
    fn build_execution_providers_cuda_nonempty() {
        assert_eq!(build_execution_providers(&ExecutionPreference::CudaThenCpu).len(), 1);
    }
}
