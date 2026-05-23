//! v0.4 N-03 cutover — TypeScript-facing `RerankerConfig` napi class,
//! rewritten on `lunaris-rerank-native::NativeReranker` (+
//! `NativeQuantizedReranker` under `reranker-gguf`). The retired
//! `RerankerConfig.fastembed()` factory is replaced by
//! `RerankerConfig.native()` and `.nativeQuantized()`.

use std::path::PathBuf;
use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use lunaris_core::LunarisError;
use lunaris_rerank::{NoopReranker, Reranker};
#[cfg(feature = "reranker-gguf")]
use lunaris_rerank_native::{NativeQuantizedReranker, NativeQuantizedRerankerOpts};
use lunaris_rerank_native::{NativeReranker, NativeRerankerOpts};

use crate::errors::napi_err;

/// Opaque container around a resolved [`lunaris_rerank::Reranker`]. Constructed
/// via the `#[napi(factory)]` methods; consumed by
/// [`crate::Lunaris::withReranker`].
#[napi]
pub struct RerankerConfig {
    pub(crate) inner: Arc<dyn Reranker>,
}

#[napi]
impl RerankerConfig {
    /// FP32 candle `NativeReranker` backed by `BAAI/bge-reranker-v2-m3`.
    /// `modelDir` defaults to `<cache>/lunaris/models/bge-reranker-v2-m3/`.
    #[napi(factory)]
    pub fn native(opts: Option<NativeRerankerConfigOpts>) -> Result<Self> {
        let opts = opts.unwrap_or_default();
        let dir = opts.model_dir.map(PathBuf::from).unwrap_or_else(default_bge_dir);
        let opts = NativeRerankerOpts {
            weights_path: dir.join("model.safetensors"),
            tokenizer_path: dir.join("tokenizer.json"),
            config_path: dir.join("config.json"),
            device: candle_core::Device::Cpu,
        };
        let r = NativeReranker::open(opts).map_err(|e| napi_err(LunarisError::from(e)))?;
        Ok(Self { inner: Arc::new(r) })
    }

    /// Q4_K_M GGUF `NativeQuantizedReranker` — requires the napi cdylib built
    /// with `reranker-gguf`.
    #[napi(factory, js_name = "nativeQuantized")]
    #[cfg(feature = "reranker-gguf")]
    pub fn native_quantized(opts: NativeQuantizedRerankerConfigOpts) -> Result<Self> {
        let dir = opts.model_dir.map(PathBuf::from).unwrap_or_else(default_bge_dir);
        let opts = NativeQuantizedRerankerOpts {
            gguf_path: PathBuf::from(opts.gguf_path),
            tokenizer_path: dir.join("tokenizer.json"),
            config_path: dir.join("config.json"),
            device: candle_core::Device::Cpu,
        };
        let r = NativeQuantizedReranker::open(opts).map_err(|e| napi_err(LunarisError::from(e)))?;
        Ok(Self { inner: Arc::new(r) })
    }

    #[napi(factory, js_name = "nativeQuantized")]
    #[cfg(not(feature = "reranker-gguf"))]
    #[allow(unused_variables)]
    pub fn native_quantized(opts: NativeQuantizedRerankerConfigOpts) -> Result<Self> {
        Err(napi::Error::new(
            napi::Status::InvalidArg,
            "RerankerConfig.nativeQuantized() requires the lunaris-ts cdylib to be built \
             with the `reranker-gguf` feature.",
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
pub struct NativeRerankerConfigOpts {
    /// Override for the bge-reranker model directory. Default:
    /// `<cache>/lunaris/models/bge-reranker-v2-m3/`.
    pub model_dir: Option<String>,
}

#[napi(object)]
pub struct NativeQuantizedRerankerConfigOpts {
    /// Path to the Q4_K_M GGUF artifact.
    pub gguf_path: String,
    pub model_dir: Option<String>,
}

fn default_bge_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Caches"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("lunaris").join("models").join("bge-reranker-v2-m3")
}
