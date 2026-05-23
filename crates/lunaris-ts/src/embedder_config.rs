//! v0.4 N-03 cutover — TypeScript-facing `EmbedderConfig` napi class,
//! rewritten on `lunaris-embed-native::NativeEmbedder` (+
//! `NativeQuantizedEmbedder` under `embedder-gguf`). The retired
//! `EmbedderConfig.fastembed/.fromOnnxBytes/.fromOnnxPath/.ollama` factories
//! are replaced by `EmbedderConfig.native()` and
//! `EmbedderConfig.nativeQuantized()` — see
//! `docs/migration/0.3-to-0.4-native-default.md` for the TypeScript
//! migration recipe.
//!
//! Handwritten (NOT codegen-managed). Mirrors the Python sibling
//! `crates/lunaris-py/src/embedder_config.rs` modulo camelCase.

use std::path::PathBuf;
use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use lunaris_core::{Embedder, LunarisError, NOOP_DEFAULT_DIM, NoopEmbedder};
use lunaris_embed_native::{GRANITE_R2_DIM, NativeEmbedder, NativeEmbedderOpts};
#[cfg(feature = "embedder-gguf")]
use lunaris_embed_native::{NativeQuantizedEmbedder, NativeQuantizedEmbedderOpts};

use crate::errors::napi_err;

/// Opaque container around a resolved [`lunaris_core::Embedder`]. Constructed
/// via one of the `#[napi(factory)]` methods; consumed by
/// [`crate::Lunaris::withEmbedder`].
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

    /// FP16 candle `NativeEmbedder` backed by
    /// `ibm-granite/granite-embedding-311m-multilingual-r2` (768-d).
    ///
    /// `modelDir` defaults to
    /// `~/Library/Caches/lunaris/models/granite-embedding-311m-multilingual-r2/`
    /// on macOS (resolves via `dirs::cache_dir()` on other platforms).
    #[napi(factory)]
    pub fn native(opts: Option<NativeConfigOpts>) -> Result<Self> {
        let opts = opts.unwrap_or_default();
        let dir = opts.model_dir.map(PathBuf::from).unwrap_or_else(default_granite_dir);
        let opts = NativeEmbedderOpts {
            weights_path: dir.join("model.safetensors"),
            tokenizer_path: dir.join("tokenizer.json"),
            config_path: dir.join("config.json"),
            device: candle_core::Device::Cpu,
        };
        let e = NativeEmbedder::open(opts).map_err(|e| napi_err(LunarisError::from(e)))?;
        Ok(Self { inner: Arc::new(e), declared_dim: GRANITE_R2_DIM as u32 })
    }

    /// Q4_K_M GGUF `NativeQuantizedEmbedder` — requires the napi cdylib
    /// to be built with the `embedder-gguf` feature.
    #[napi(factory, js_name = "nativeQuantized")]
    #[cfg(feature = "embedder-gguf")]
    pub fn native_quantized(opts: NativeQuantizedConfigOpts) -> Result<Self> {
        let dir = opts.model_dir.map(PathBuf::from).unwrap_or_else(default_granite_dir);
        let opts = NativeQuantizedEmbedderOpts {
            gguf_path: PathBuf::from(opts.gguf_path),
            tokenizer_path: dir.join("tokenizer.json"),
            config_path: dir.join("config.json"),
            device: candle_core::Device::Cpu,
        };
        let e = NativeQuantizedEmbedder::open(opts).map_err(|e| napi_err(LunarisError::from(e)))?;
        Ok(Self { inner: Arc::new(e), declared_dim: GRANITE_R2_DIM as u32 })
    }

    /// Stub raising `InvalidArg` when the cdylib was built without
    /// `embedder-gguf`. Surfaces a clear error instead of `undefined`.
    #[napi(factory, js_name = "nativeQuantized")]
    #[cfg(not(feature = "embedder-gguf"))]
    #[allow(unused_variables)]
    pub fn native_quantized(opts: NativeQuantizedConfigOpts) -> Result<Self> {
        Err(napi::Error::new(
            napi::Status::InvalidArg,
            "EmbedderConfig.nativeQuantized() requires the lunaris-ts cdylib to be built \
             with the `embedder-gguf` feature.",
        ))
    }

    /// Output dimensionality declared by the operator at config time.
    /// For native paths this is `GRANITE_R2_DIM = 768`.
    #[napi(getter)]
    pub fn declared_dim(&self) -> u32 {
        self.declared_dim
    }
}

#[napi(object)]
#[derive(Default)]
pub struct NativeConfigOpts {
    /// Override for the granite-r2 model directory. Default:
    /// `<cache>/lunaris/models/granite-embedding-311m-multilingual-r2/`.
    pub model_dir: Option<String>,
}

#[napi(object)]
pub struct NativeQuantizedConfigOpts {
    /// Path to the Q4_K_M GGUF artifact.
    pub gguf_path: String,
    /// Override for the model directory holding `tokenizer.json` +
    /// `config.json`. Default: same as `EmbedderConfig.native()`.
    pub model_dir: Option<String>,
}

fn default_granite_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Caches"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("lunaris").join("models").join("granite-embedding-311m-multilingual-r2")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_default_dim_matches_granite_r2() {
        let cfg = EmbedderConfig::noop(None);
        assert_eq!(cfg.declared_dim, GRANITE_R2_DIM as u32);
        assert_eq!(cfg.declared_dim, 768);
    }

    #[test]
    fn default_granite_dir_ends_with_canonical_name() {
        let p = default_granite_dir();
        assert!(
            p.ends_with("lunaris/models/granite-embedding-311m-multilingual-r2"),
            "got {}",
            p.display()
        );
    }
}
