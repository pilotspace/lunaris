//! Plan 21-02 Task 1 — TypeScript-facing `EmbedderConfig` napi class.
//!
//! Handwritten (NOT codegen-managed). Mirrors the Python sibling
//! `crates/lunaris-py/src/embedder_config.rs` from Plan 21-01 modulo
//! camelCase. Four factory methods:
//!
//! - `EmbedderConfig.fastembed({ cacheDir?, execution?, showDownloadProgress? })`
//! - `EmbedderConfig.fromOnnxBytes({ onnxBytes, tokenizerBytes, dim, pooling?, ... })`
//! - `EmbedderConfig.fromOnnxPath({ onnxPath, tokenizerPath, dim, pooling?, ... })`
//! - `EmbedderConfig.ollama({ endpoint?, model?, dim? })`
//!
//! Each returns an opaque `EmbedderConfig` holding `Arc<dyn Embedder>` plus
//! the operator-declared output dimensionality. The BYO-ONNX factories wrap
//! the resolved embedder in [`DimValidatingEmbedder`] so a mismatched
//! `dim` declaration surfaces a clear error on the first `embed_batch`
//! call rather than corrupting the vector index silently.
//!
//! ## FFI cliff
//!
//! TS callers cannot implement the `Embedder` trait from JavaScript — that
//! would require per-call FFI callbacks, which napi-rs explicitly discourages
//! for hot paths. The "roll-your-own" escape hatch is Rust-crate-only;
//! Phase 21 surfaces presets + BYO-ONNX only.
//!
//! ## SDK-shared invariant
//!
//! Keep [`DimValidatingEmbedder`] in sync with the Python sibling at
//! `crates/lunaris-py/src/embedder_config.rs`. Both files duplicate the
//! ~30-line wrapper deliberately to avoid introducing a `lunaris-sdk-shared`
//! crate for two callers. If the implementation drifts, the Plan 21-03
//! cross-SDK conformance test catches it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use napi::bindgen_prelude::*;
use napi_derive::napi;

use lunaris_core::{Embedder, LunarisError, StorageError};
use lunaris_embed::fastembed::{
    FastembedEmbedder, FastembedEmbedderOpts, FastembedUserDefinedOpts, PoolingMode,
};
use lunaris_embed::fastembed_exec::ExecutionPreference;
use lunaris_embed::ollama::{OllamaEmbedder, OllamaEmbedderOpts};

use crate::errors::napi_err;

const DEFAULT_OLLAMA_DIM: u32 = 768;

// =====================================================================
// EmbedderConfig — opaque napi holder.
// =====================================================================

/// Opaque container around a resolved [`lunaris_core::Embedder`]. Constructed
/// via one of the four `#[napi(factory)]` methods; consumed by
/// [`crate::Lunaris::withEmbedder`].
///
/// The inner `Arc<dyn Embedder>` is intentionally `pub(crate)` so only the
/// host crate's `with_embedder` napi extension can read it — TS callers
/// see an opaque token.
#[napi]
pub struct EmbedderConfig {
    pub(crate) inner: Arc<dyn Embedder>,
    /// Operator-declared output dim. For the preset paths this is the model
    /// constant (768); for BYO this is the `dim` the caller passed and
    /// gets re-checked on first `embed_batch` via [`DimValidatingEmbedder`].
    pub(crate) declared_dim: u32,
}

#[napi]
impl EmbedderConfig {
    /// Build an [`EmbedderConfig`] from the fastembed EmbeddingGemma300M
    /// preset. All opts are optional.
    #[napi(factory)]
    pub fn fastembed(opts: Option<FastembedConfigOpts>) -> Result<Self> {
        let opts = opts.unwrap_or_default();
        let cache_dir = opts.cache_dir.map(PathBuf::from);
        let execution = parse_execution(opts.execution.as_deref())?;
        let show_download_progress = opts.show_download_progress.unwrap_or(false);

        let embedder = FastembedEmbedder::new(FastembedEmbedderOpts {
            cache_dir,
            show_download_progress,
            execution,
        })
        .map_err(napi_err)?;
        let dim = embedder.dim() as u32;
        Ok(Self { inner: Arc::new(embedder), declared_dim: dim })
    }

    /// Bring-your-own ONNX model via in-memory bytes (e.g., fetched from S3).
    /// `dim` MUST match the ONNX model's output shape; a mismatch surfaces
    /// on the first `embed_batch` call.
    #[napi(factory, js_name = "fromOnnxBytes")]
    pub fn from_onnx_bytes(opts: FromOnnxBytesOpts) -> Result<Self> {
        let pooling = parse_pooling(opts.pooling.as_deref())?;
        let execution = parse_execution(opts.execution.as_deref())?;
        let dim = opts.dim;
        if dim == 0 {
            return Err(napi_err(LunarisError::Storage(StorageError::Backend(
                "fastembed: from_onnx_bytes called with dim = 0".into(),
            ))));
        }

        let embedder = FastembedEmbedder::from_user_defined(FastembedUserDefinedOpts {
            onnx_file: opts.onnx_bytes.to_vec(),
            tokenizer_file: opts.tokenizer_bytes.to_vec(),
            tokenizer_config_file: opts.tokenizer_config_bytes.map(|b| b.to_vec()),
            special_tokens_map_file: opts.special_tokens_map_bytes.map(|b| b.to_vec()),
            config_file: opts.config_bytes.map(|b| b.to_vec()),
            dim: dim as usize,
            pooling,
            execution,
            max_length: lunaris_embed::fastembed::FASTEMBED_GEMMA_MAX_TOKENS,
        })
        .map_err(napi_err)?;

        let wrapped: Arc<dyn Embedder> = Arc::new(DimValidatingEmbedder {
            inner: Arc::new(embedder),
            declared_dim: dim as usize,
            validated: AtomicBool::new(false),
        });
        Ok(Self { inner: wrapped, declared_dim: dim })
    }

    /// Bring-your-own ONNX model from disk. Reads the bytes via
    /// `std::fs::read` (operator-trusted paths only) and delegates to the
    /// same constructor as `fromOnnxBytes`.
    #[napi(factory, js_name = "fromOnnxPath")]
    pub fn from_onnx_path(opts: FromOnnxPathOpts) -> Result<Self> {
        let onnx_bytes = read_file(&opts.onnx_path, "onnx_path")?;
        let tokenizer_bytes = read_file(&opts.tokenizer_path, "tokenizer_path")?;
        let tokenizer_config_bytes = opts
            .tokenizer_config_path
            .as_ref()
            .map(|p| read_file(p, "tokenizer_config_path"))
            .transpose()?;
        let special_tokens_map_bytes = opts
            .special_tokens_map_path
            .as_ref()
            .map(|p| read_file(p, "special_tokens_map_path"))
            .transpose()?;
        let config_bytes =
            opts.config_path.as_ref().map(|p| read_file(p, "config_path")).transpose()?;

        Self::from_onnx_bytes(FromOnnxBytesOpts {
            onnx_bytes: onnx_bytes.into(),
            tokenizer_bytes: tokenizer_bytes.into(),
            dim: opts.dim,
            pooling: opts.pooling,
            tokenizer_config_bytes: tokenizer_config_bytes.map(Into::into),
            special_tokens_map_bytes: special_tokens_map_bytes.map(Into::into),
            config_bytes: config_bytes.map(Into::into),
            execution: opts.execution,
        })
    }

    /// Build an [`EmbedderConfig`] backed by Ollama's `/api/embed` HTTP API.
    /// Defaults: `endpoint=http://localhost:11434`, `model=embeddinggemma`,
    /// `dim=768`. The constructor only builds a `reqwest::Client` — it does
    /// not contact Ollama until the first `embed_batch`.
    #[napi(factory)]
    pub fn ollama(opts: Option<OllamaConfigOpts>) -> Result<Self> {
        let opts = opts.unwrap_or_default();
        let dim = opts.dim.unwrap_or(DEFAULT_OLLAMA_DIM);
        if dim == 0 {
            return Err(napi_err(LunarisError::Storage(StorageError::Backend(
                "ollama: dim must be > 0".into(),
            ))));
        }
        let defaults = OllamaEmbedderOpts::default();
        let embedder = OllamaEmbedder::new(OllamaEmbedderOpts {
            endpoint: opts.endpoint.unwrap_or(defaults.endpoint),
            model: opts.model.unwrap_or(defaults.model),
            dim: dim as usize,
        })
        .map_err(napi_err)?;
        Ok(Self { inner: Arc::new(embedder), declared_dim: dim })
    }

    /// Output dimensionality declared by the operator at config time.
    /// For preset paths this matches the model constant; for BYO this is
    /// what the caller passed (and what [`DimValidatingEmbedder`] enforces).
    #[napi(getter)]
    pub fn declared_dim(&self) -> u32 {
        self.declared_dim
    }
}

// =====================================================================
// Opts structs — `#[napi(object)]` so napi-rs emits TS interfaces.
// =====================================================================

#[napi(object)]
#[derive(Default)]
pub struct FastembedConfigOpts {
    pub cache_dir: Option<String>,
    /// `"cpu"` | `"coreml"` | `"cuda"` — case-insensitive. Default `cpu`.
    pub execution: Option<String>,
    pub show_download_progress: Option<bool>,
}

#[napi(object)]
pub struct FromOnnxBytesOpts {
    pub onnx_bytes: Buffer,
    pub tokenizer_bytes: Buffer,
    pub dim: u32,
    /// `"mean"` | `"cls"`. Default `mean`.
    pub pooling: Option<String>,
    pub tokenizer_config_bytes: Option<Buffer>,
    pub special_tokens_map_bytes: Option<Buffer>,
    pub config_bytes: Option<Buffer>,
    pub execution: Option<String>,
}

#[napi(object)]
pub struct FromOnnxPathOpts {
    pub onnx_path: String,
    pub tokenizer_path: String,
    pub dim: u32,
    pub pooling: Option<String>,
    pub tokenizer_config_path: Option<String>,
    pub special_tokens_map_path: Option<String>,
    pub config_path: Option<String>,
    pub execution: Option<String>,
}

#[napi(object)]
#[derive(Default)]
pub struct OllamaConfigOpts {
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub dim: Option<u32>,
}

// =====================================================================
// Parsers — shared with reranker_config.rs via pub(crate).
// =====================================================================

/// Parse a TS-facing execution string into an [`ExecutionPreference`].
/// `None` → `Cpu`. Unknown values are rejected with an InvalidArg napi
/// error (matches the Python sibling's `PyValueError`).
pub(crate) fn parse_execution(s: Option<&str>) -> Result<ExecutionPreference> {
    let raw = match s {
        None => return Ok(ExecutionPreference::Cpu),
        Some(v) => v.trim(),
    };
    match raw.to_ascii_lowercase().as_str() {
        "" | "cpu" => Ok(ExecutionPreference::Cpu),
        #[cfg(feature = "fastembed-coreml")]
        "coreml" | "coreml_then_cpu" | "coreml-then-cpu" => {
            Ok(ExecutionPreference::CoreMlThenCpu)
        }
        #[cfg(feature = "fastembed-cuda")]
        "cuda" | "cuda_then_cpu" | "cuda-then-cpu" => Ok(ExecutionPreference::CudaThenCpu),
        // For CPU-only builds, accept the value silently as Cpu rather than
        // error — mirrors the env-var path's `tracing::warn + Cpu` semantics
        // so SDK callers can ship cross-platform config.
        #[cfg(not(feature = "fastembed-coreml"))]
        "coreml" | "coreml_then_cpu" | "coreml-then-cpu" => {
            tracing::warn!(
                requested = raw,
                "EmbedderConfig: execution='coreml' requested but lunaris-ts was built without `fastembed-coreml` — using cpu"
            );
            Ok(ExecutionPreference::Cpu)
        }
        #[cfg(not(feature = "fastembed-cuda"))]
        "cuda" | "cuda_then_cpu" | "cuda-then-cpu" => {
            tracing::warn!(
                requested = raw,
                "EmbedderConfig: execution='cuda' requested but lunaris-ts was built without `fastembed-cuda` — using cpu"
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

/// Parse a pooling string into a [`PoolingMode`]. `None` → `Mean`.
fn parse_pooling(s: Option<&str>) -> Result<PoolingMode> {
    let raw = match s {
        None => return Ok(PoolingMode::Mean),
        Some(v) => v.trim(),
    };
    match raw.to_ascii_lowercase().as_str() {
        "" | "mean" => Ok(PoolingMode::Mean),
        "cls" => Ok(PoolingMode::Cls),
        other => Err(napi::Error::new(
            napi::Status::InvalidArg,
            format!(
                "VALIDATE: unknown pooling mode '{other}' — expected 'mean' or 'cls'"
            ),
        )),
    }
}

/// Read a file into bytes; map I/O errors through the napi error translator
/// with a deterministic `STORAGE:` prefix and the operator-supplied path so
/// the failure is actionable.
fn read_file(path: &str, field: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| {
        napi_err(LunarisError::Storage(StorageError::Backend(format!(
            "fastembed: failed to read {field}='{path}': {e}"
        ))))
    })
}

// =====================================================================
// DimValidatingEmbedder — first-call dim check.
//
// SDK-shared: keep in sync with crates/lunaris-py/src/embedder_config.rs
// =====================================================================

/// Wraps an [`Embedder`] and verifies that the first batch's vector length
/// matches the operator-declared `dim`. Subsequent calls bypass the check
/// (the `AtomicBool` flips on first validation).
struct DimValidatingEmbedder {
    inner: Arc<dyn Embedder>,
    declared_dim: usize,
    validated: AtomicBool,
}

#[async_trait]
impl Embedder for DimValidatingEmbedder {
    fn dim(&self) -> usize {
        self.declared_dim
    }

    async fn embed_batch(&self, inputs: &[&str]) -> std::result::Result<Vec<Vec<f32>>, LunarisError> {
        let out = self.inner.embed_batch(inputs).await?;
        if !self.validated.swap(true, Ordering::Relaxed)
            && let Some(first) = out.first()
            && first.len() != self.declared_dim
        {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "declared dim {} does not match observed dim {} — check the ONNX model output shape",
                self.declared_dim,
                first.len()
            ))));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_execution_none_defaults_to_cpu() {
        assert!(matches!(parse_execution(None).unwrap(), ExecutionPreference::Cpu));
    }

    #[test]
    fn parse_execution_cpu_string_ok() {
        assert!(matches!(parse_execution(Some("cpu")).unwrap(), ExecutionPreference::Cpu));
        assert!(matches!(parse_execution(Some("CPU")).unwrap(), ExecutionPreference::Cpu));
        assert!(matches!(parse_execution(Some("")).unwrap(), ExecutionPreference::Cpu));
    }

    #[test]
    fn parse_execution_bogus_rejected() {
        let err = parse_execution(Some("bogus")).unwrap_err();
        assert!(err.reason.contains("execution"), "reason was: {}", err.reason);
    }

    #[test]
    fn parse_pooling_none_defaults_to_mean() {
        assert_eq!(parse_pooling(None).unwrap(), PoolingMode::Mean);
    }

    #[test]
    fn parse_pooling_cls_ok() {
        assert_eq!(parse_pooling(Some("cls")).unwrap(), PoolingMode::Cls);
    }

    #[test]
    fn parse_pooling_bogus_rejected() {
        let err = parse_pooling(Some("bogus")).unwrap_err();
        assert!(err.reason.contains("pooling"), "reason was: {}", err.reason);
    }
}
