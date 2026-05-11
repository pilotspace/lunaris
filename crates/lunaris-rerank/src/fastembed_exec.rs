//! ORT execution-provider plumbing for the fastembed reranker backend (Phase
//! 20 Plan 20-01). Sibling of `fastembed.rs`; rationale, contract and shape
//! mirror `lunaris_embed::fastembed_exec`.
//!
//! Duplication-vs-extraction call: the embedder and reranker each carry their
//! own copy of this module (~150 lines per crate). Crossing the lunaris-embed
//! → lunaris-rerank dependency boundary just to share the enum + parser would
//! couple two otherwise-independent backend crates for ~150 lines of code.
//! Plan 20-01's guidance: extract only when duplication > 50 lines AND a
//! cross-cutting consumer exists. Neither holds today.

#[cfg(any(feature = "fastembed-coreml", feature = "fastembed-cuda"))]
use fastembed::ExecutionProviderDispatch;

/// Environment variable that selects the ORT execution provider preference.
/// **Same name** as `lunaris_embed::fastembed::FASTEMBED_EXECUTION_ENV` — set
/// once, both backends adapt.
pub const FASTEMBED_EXECUTION_ENV: &str = "LUNARIS_FASTEMBED_EXECUTION";

/// Operator-facing preference for ORT execution provider on the reranker
/// backend. Identical surface to
/// `lunaris_embed::fastembed_exec::ExecutionPreference`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ExecutionPreference {
    #[default]
    Cpu,
    #[cfg(feature = "fastembed-coreml")]
    CoreMlThenCpu,
    #[cfg(feature = "fastembed-cuda")]
    CudaThenCpu,
}

/// Parse from a string; unknown values warn and resolve to `Cpu`.
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
                "LUNARIS_FASTEMBED_EXECUTION=coreml but lunaris-rerank was built without `fastembed-coreml` — defaulting to cpu"
            );
            ExecutionPreference::Cpu
        }
        #[cfg(not(feature = "fastembed-cuda"))]
        "cuda" | "cuda_then_cpu" | "cuda-then-cpu" => {
            tracing::warn!(
                requested = %s,
                "LUNARIS_FASTEMBED_EXECUTION=cuda but lunaris-rerank was built without `fastembed-cuda` — defaulting to cpu"
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

/// Resolve from the process environment.
pub fn execution_from_env() -> ExecutionPreference {
    match std::env::var(FASTEMBED_EXECUTION_ENV) {
        Ok(v) if !v.is_empty() => parse_execution(&v),
        _ => ExecutionPreference::Cpu,
    }
}

#[cfg(any(feature = "fastembed-coreml", feature = "fastembed-cuda"))]
#[allow(unused_variables)]
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

#[cfg(not(any(feature = "fastembed-coreml", feature = "fastembed-cuda")))]
pub(crate) fn build_execution_providers(
    _pref: &ExecutionPreference,
) -> Vec<fastembed::ExecutionProviderDispatch> {
    Vec::new()
}

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
        assert_eq!(parse_execution("cpu"), ExecutionPreference::Cpu);
        assert_eq!(parse_execution("CPU"), ExecutionPreference::Cpu);
    }

    #[cfg(feature = "fastembed-coreml")]
    #[test]
    fn execution_pref_coreml_parses_when_feature_on() {
        assert_eq!(parse_execution("coreml"), ExecutionPreference::CoreMlThenCpu);
    }

    #[cfg(feature = "fastembed-cuda")]
    #[test]
    fn execution_pref_cuda_parses_when_feature_on() {
        assert_eq!(parse_execution("cuda"), ExecutionPreference::CudaThenCpu);
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
