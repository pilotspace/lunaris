//! llama.cpp-only cutover (2026-07, Phase C) — TypeScript-facing
//! `EmbedderConfig` napi class, rewritten on
//! `lunaris-llamacpp::LlamaCppEmbedder` (granite-r2 Q4_K_M GGUF). The
//! retired v0.4 `EmbedderConfig.native()` / `EmbedderConfig.nativeQuantized()`
//! factories (candle) fail loudly with a migration hint; the supported
//! factories are `EmbedderConfig.llamacpp()` and `EmbedderConfig.noop()`.
//!
//! Handwritten (NOT codegen-managed). Mirrors the Python sibling
//! `crates/lunaris-py/src/embedder_config.rs` modulo camelCase.

use std::path::PathBuf;
use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use lunaris_core::{Embedder, NOOP_DEFAULT_DIM, NoopEmbedder};

#[cfg(feature = "llamacpp")]
use crate::errors::napi_err;
#[cfg(feature = "llamacpp")]
use lunaris_core::LunarisError;

/// Opaque container around a resolved [`lunaris_core::Embedder`]. Constructed
/// via one of the `#[napi(factory)]` methods; consumed by
/// `crate::Lunaris::withEmbedder`.
#[napi]
pub struct EmbedderConfig {
    pub(crate) inner: Arc<dyn Embedder>,
    pub(crate) declared_dim: u32,
}

#[napi]
impl EmbedderConfig {
    /// Zero-vector embedder — metadata-only ingest / BYO vector / unit-test
    /// path. Default dim matches granite-r2 (768).
    #[napi(factory)]
    pub fn noop(dim: Option<u32>) -> Self {
        let dim = dim.unwrap_or(NOOP_DEFAULT_DIM as u32);
        Self { inner: Arc::new(NoopEmbedder::new(dim as usize)), declared_dim: dim }
    }

    /// llama.cpp `LlamaCppEmbedder` backed by the granite-r2 Q4_K_M GGUF
    /// (768-d).
    ///
    /// `ggufPath` defaults to the staged artifact
    /// `~/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf`.
    #[napi(factory)]
    #[cfg(feature = "llamacpp")]
    pub fn llamacpp(opts: Option<LlamaCppConfigOpts>) -> Result<Self> {
        let opts = opts.unwrap_or_default();
        let gguf_path = opts.gguf_path.map(PathBuf::from).unwrap_or_else(default_embedder_gguf);
        let opts = lunaris_llamacpp::LlamaCppEmbedderOpts { gguf_path, ..Default::default() };
        let e = lunaris_llamacpp::LlamaCppEmbedder::open(opts)
            .map_err(|e| napi_err(LunarisError::from(e)))?;
        let dim = e.dim() as u32;
        Ok(Self { inner: Arc::new(e), declared_dim: dim })
    }

    /// Stub raising `InvalidArg` when the cdylib was built without the
    /// `llamacpp` feature (Tier-0 build).
    #[napi(factory)]
    #[cfg(not(feature = "llamacpp"))]
    #[allow(unused_variables)]
    pub fn llamacpp(opts: Option<LlamaCppConfigOpts>) -> Result<Self> {
        Err(napi::Error::new(
            napi::Status::InvalidArg,
            "EmbedderConfig.llamacpp() requires the lunaris cdylib to be built with the \
             `llamacpp` feature (this is a Tier-0 no-inference build). Use \
             EmbedderConfig.noop() or install a full build.",
        ))
    }

    /// RETIRED (llama.cpp-only cutover): the candle `NativeEmbedder` was
    /// deleted. Use `EmbedderConfig.llamacpp()` instead.
    #[napi(factory)]
    #[allow(unused_variables)]
    pub fn native(opts: Option<NativeConfigOpts>) -> Result<Self> {
        Err(napi::Error::new(
            napi::Status::InvalidArg,
            "EmbedderConfig.native() was removed in the llama.cpp-only cutover (v0.6): \
             the candle FP16 embedder no longer exists. Use EmbedderConfig.llamacpp() \
             (granite-r2 Q4_K_M GGUF, same 768-d vectors).",
        ))
    }

    /// RETIRED (llama.cpp-only cutover): the candle quantized embedder was
    /// deleted. Use `EmbedderConfig.llamacpp()` instead.
    #[napi(factory, js_name = "nativeQuantized")]
    #[allow(unused_variables)]
    pub fn native_quantized(opts: NativeQuantizedConfigOpts) -> Result<Self> {
        Err(napi::Error::new(
            napi::Status::InvalidArg,
            "EmbedderConfig.nativeQuantized() was removed in the llama.cpp-only cutover \
             (v0.6). Use EmbedderConfig.llamacpp({ ggufPath }) — same GGUF artifact, \
             served by llama.cpp.",
        ))
    }

    /// Output dimensionality declared by the operator at config time.
    /// For the llamacpp path this is granite-r2's 768.
    #[napi(getter)]
    pub fn declared_dim(&self) -> u32 {
        self.declared_dim
    }
}

#[napi(object)]
#[derive(Default)]
pub struct LlamaCppConfigOpts {
    /// Path to the GGUF artifact. Default:
    /// `~/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf`.
    pub gguf_path: Option<String>,
}

#[napi(object)]
#[derive(Default)]
pub struct NativeConfigOpts {
    /// RETIRED — kept so existing callers get the migration error, not a
    /// TypeScript signature break.
    pub model_dir: Option<String>,
}

#[napi(object)]
pub struct NativeQuantizedConfigOpts {
    /// RETIRED — see `EmbedderConfig.llamacpp()`.
    pub gguf_path: String,
    /// RETIRED — see `EmbedderConfig.llamacpp()`.
    pub model_dir: Option<String>,
}

/// `~/.lunaris/models/` — the GGUF staging directory shared with the
/// umbrella resolver and the `stage-models` tool.
#[cfg(feature = "llamacpp")]
pub(crate) fn lunaris_models_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".lunaris")
        .join("models")
}

#[cfg(feature = "llamacpp")]
fn default_embedder_gguf() -> PathBuf {
    lunaris_models_dir().join("granite-embedding-311m-multilingual-r2.Q4_K_M.gguf")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_default_dim_matches_granite_r2() {
        let cfg = EmbedderConfig::noop(None);
        assert_eq!(cfg.declared_dim, 768);
    }

    #[test]
    fn retired_native_factories_fail_loudly() {
        assert!(EmbedderConfig::native(None).is_err());
        assert!(
            EmbedderConfig::native_quantized(NativeQuantizedConfigOpts {
                gguf_path: "x.gguf".into(),
                model_dir: None,
            })
            .is_err()
        );
    }

    #[cfg(feature = "llamacpp")]
    #[test]
    fn default_embedder_gguf_ends_with_staged_name() {
        let p = default_embedder_gguf();
        assert!(
            p.ends_with(".lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf"),
            "got {}",
            p.display()
        );
    }
}
