//! Plan 21-02 Task 2 — TypeScript-facing `RerankerConfig` napi class.
//!
//! Handwritten (NOT codegen-managed). Mirrors the Python sibling
//! `crates/lunaris-py/src/reranker_config.rs` from Plan 21-01 modulo
//! camelCase. Two factory methods:
//!
//! - `RerankerConfig.fastembed({ cacheDir?, execution?, showDownloadProgress? })`
//! - `RerankerConfig.noop()`  — the RETRIEVE-06 fallback
//!
//! ## BYO-ONNX scope (Plan 21-01 Task 1 outcome — Outcome B)
//!
//! fastembed 5.13.4 upstream exposes `TextRerank::try_new_from_user_defined`,
//! but Lunaris's `lunaris-rerank::FastembedReranker` does NOT yet wrap it
//! (Phase 19-04 / 20-01 left it as a follow-up). The SDK can only wrap what
//! the Rust crate exposes today, so 21-02 ships `fastembed()` + `noop()` only.
//!
//! Add `from_onnx_bytes` / `from_onnx_path` here once `FastembedReranker`
//! grows a `from_user_defined` constructor. Plan 21-03's cross-SDK parity
//! test will catch divergence between Python and TS if one side ships BYO
//! before the other.

use std::path::PathBuf;
use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use lunaris_rerank::Reranker;
use lunaris_rerank::fastembed::{ExecutionPreference, FastembedReranker, FastembedRerankerOpts};
use lunaris_rerank::noop::NoopReranker;

use crate::errors::napi_err;

/// Opaque container around a resolved [`lunaris_rerank::Reranker`]. Constructed
/// via one of the `#[napi(factory)]` methods; consumed by
/// [`crate::Lunaris::withReranker`].
#[napi]
pub struct RerankerConfig {
    pub(crate) inner: Arc<dyn Reranker>,
}

#[napi]
impl RerankerConfig {
    /// Build a [`RerankerConfig`] backed by the fastembed BGE-Reranker-v2-m3
    /// cross-encoder preset. All opts are optional.
    #[napi(factory)]
    pub fn fastembed(opts: Option<FastembedRerankerConfigOpts>) -> Result<Self> {
        let opts = opts.unwrap_or_default();
        let cache_dir = opts.cache_dir.map(PathBuf::from);
        let execution = parse_rerank_execution(opts.execution.as_deref())?;
        let show_download_progress = opts.show_download_progress.unwrap_or(false);
        let reranker = FastembedReranker::new(FastembedRerankerOpts {
            cache_dir,
            show_download_progress,
            execution,
        })
        .map_err(napi_err)?;
        Ok(Self { inner: Arc::new(reranker) })
    }

    /// RETRIEVE-06 fallback — passes candidates through with their original
    /// scores. Used when (a) the per-batch rerank budget is too tight on the
    /// deployment hardware, or (b) the operator wants the BM25/vector fusion
    /// score directly with no cross-encoder rescoring.
    #[napi(factory)]
    pub fn noop() -> Self {
        Self { inner: Arc::new(NoopReranker) }
    }
}

#[napi(object)]
#[derive(Default)]
pub struct FastembedRerankerConfigOpts {
    pub cache_dir: Option<String>,
    pub execution: Option<String>,
    pub show_download_progress: Option<bool>,
}

/// Parse a TS-facing execution string into the reranker's
/// [`ExecutionPreference`]. Same semantics as
/// [`crate::embedder_config::parse_execution`] but produces the parallel
/// enum from `lunaris_rerank`. `None` → `Cpu`; unknown values are rejected
/// with an `InvalidArg` napi error.
fn parse_rerank_execution(s: Option<&str>) -> Result<ExecutionPreference> {
    let raw = match s {
        None => return Ok(ExecutionPreference::Cpu),
        Some(v) => v.trim(),
    };
    match raw.to_ascii_lowercase().as_str() {
        "" | "cpu" => Ok(ExecutionPreference::Cpu),
        #[cfg(feature = "fastembed-coreml")]
        "coreml" | "coreml_then_cpu" | "coreml-then-cpu" => Ok(ExecutionPreference::CoreMlThenCpu),
        #[cfg(feature = "fastembed-cuda")]
        "cuda" | "cuda_then_cpu" | "cuda-then-cpu" => Ok(ExecutionPreference::CudaThenCpu),
        #[cfg(not(feature = "fastembed-coreml"))]
        "coreml" | "coreml_then_cpu" | "coreml-then-cpu" => {
            tracing::warn!(
                requested = raw,
                "RerankerConfig: execution='coreml' requested but lunaris-ts was built without `fastembed-coreml` — using cpu"
            );
            Ok(ExecutionPreference::Cpu)
        }
        #[cfg(not(feature = "fastembed-cuda"))]
        "cuda" | "cuda_then_cpu" | "cuda-then-cpu" => {
            tracing::warn!(
                requested = raw,
                "RerankerConfig: execution='cuda' requested but lunaris-ts was built without `fastembed-cuda` — using cpu"
            );
            Ok(ExecutionPreference::Cpu)
        }
        other => Err(napi::Error::new(
            napi::Status::InvalidArg,
            format!(
                "VALIDATE: unknown execution preference '{other}' — expected 'cpu', 'coreml', or 'cuda'"
            ),
        )),
    }
}
