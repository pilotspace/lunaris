//! llama.cpp-only cutover (2026-07, Phase C) — TypeScript-facing
//! `RerankerConfig` napi class, rewritten on
//! `lunaris-llamacpp::LlamaCppReranker` (bge-reranker-v2-m3 Q5_K_M GGUF).
//! The retired v0.4 `RerankerConfig.native()` / `.nativeQuantized()`
//! factories (candle) fail loudly with a migration hint; the supported
//! factories are `RerankerConfig.llamacpp()` and `RerankerConfig.noop()`.

use std::path::PathBuf;
use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use lunaris_rerank::{NoopReranker, Reranker};

#[cfg(feature = "llamacpp")]
use crate::errors::napi_err;
#[cfg(feature = "llamacpp")]
use lunaris_core::LunarisError;

/// Opaque container around a resolved [`lunaris_rerank::Reranker`]. Constructed
/// via the `#[napi(factory)]` methods; consumed by
/// `crate::Lunaris::withReranker`.
#[napi]
pub struct RerankerConfig {
    pub(crate) inner: Arc<dyn Reranker>,
}

#[napi]
impl RerankerConfig {
    /// llama.cpp `LlamaCppReranker` backed by the bge-reranker-v2-m3 Q5_K_M
    /// GGUF (cross-encoder, sigmoid scores ∈ [0, 1]).
    ///
    /// `ggufPath` defaults to the staged artifact
    /// `~/.lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf`. Loads eagerly and
    /// raises on a missing/corrupt artifact (explicit construction = fail
    /// fast; the umbrella's default resolution defers the load instead).
    #[napi(factory)]
    #[cfg(feature = "llamacpp")]
    pub fn llamacpp(opts: Option<LlamaCppRerankerConfigOpts>) -> Result<Self> {
        let opts = opts.unwrap_or_default();
        let gguf_path = opts.gguf_path.map(PathBuf::from).unwrap_or_else(default_reranker_gguf);
        let opts = lunaris_llamacpp::LlamaCppRerankerOpts { gguf_path, ..Default::default() };
        let r = lunaris_llamacpp::LlamaCppReranker::open(opts)
            .map_err(|e| napi_err(LunarisError::from(e)))?;
        Ok(Self { inner: Arc::new(r) })
    }

    /// Stub raising `InvalidArg` when the cdylib was built without the
    /// `llamacpp` feature (Tier-0 build).
    #[napi(factory)]
    #[cfg(not(feature = "llamacpp"))]
    #[allow(unused_variables)]
    pub fn llamacpp(opts: Option<LlamaCppRerankerConfigOpts>) -> Result<Self> {
        Err(napi::Error::new(
            napi::Status::InvalidArg,
            "RerankerConfig.llamacpp() requires the lunaris cdylib to be built with the \
             `llamacpp` feature (this is a Tier-0 no-inference build). Use \
             RerankerConfig.noop() or install a full build.",
        ))
    }

    /// RETIRED (llama.cpp-only cutover): the candle `NativeReranker` was
    /// deleted. Use `RerankerConfig.llamacpp()` instead.
    #[napi(factory)]
    #[allow(unused_variables)]
    pub fn native(opts: Option<NativeRerankerConfigOpts>) -> Result<Self> {
        Err(napi::Error::new(
            napi::Status::InvalidArg,
            "RerankerConfig.native() was removed in the llama.cpp-only cutover (v0.6): \
             the candle FP32 reranker no longer exists. Use RerankerConfig.llamacpp() \
             (bge-reranker-v2-m3 Q5_K_M GGUF, same sigmoid score contract).",
        ))
    }

    /// RETIRED (llama.cpp-only cutover): the candle quantized reranker was
    /// deleted. Use `RerankerConfig.llamacpp()` instead.
    #[napi(factory, js_name = "nativeQuantized")]
    #[allow(unused_variables)]
    pub fn native_quantized(opts: NativeQuantizedRerankerConfigOpts) -> Result<Self> {
        Err(napi::Error::new(
            napi::Status::InvalidArg,
            "RerankerConfig.nativeQuantized() was removed in the llama.cpp-only cutover \
             (v0.6). Use RerankerConfig.llamacpp({ ggufPath }) — same GGUF artifact, \
             served by llama.cpp.",
        ))
    }

    /// RETRIEVE-06 fallback — passes candidates through with original scores.
    #[napi(factory)]
    pub fn noop() -> Self {
        Self { inner: Arc::new(NoopReranker) }
    }
}

#[napi(object)]
#[derive(Default)]
pub struct LlamaCppRerankerConfigOpts {
    /// Path to the GGUF artifact. Default:
    /// `~/.lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf`.
    pub gguf_path: Option<String>,
}

#[napi(object)]
#[derive(Default)]
pub struct NativeRerankerConfigOpts {
    /// RETIRED — kept so existing callers get the migration error, not a
    /// TypeScript signature break.
    pub model_dir: Option<String>,
}

#[napi(object)]
pub struct NativeQuantizedRerankerConfigOpts {
    /// RETIRED — see `RerankerConfig.llamacpp()`.
    pub gguf_path: String,
    /// RETIRED — see `RerankerConfig.llamacpp()`.
    pub model_dir: Option<String>,
}

#[cfg(feature = "llamacpp")]
fn default_reranker_gguf() -> PathBuf {
    crate::embedder_config::lunaris_models_dir().join("bge-reranker-v2-m3.Q5_K_M.gguf")
}
