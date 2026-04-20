//! Embedder contract tests for Phase 2 hot path.
//!
//! Default features (`candle`) run:
//!   - `missing_weights_returns_actionable_error` — the cold-start error path
//!     (no model files present) returns a `LunarisError::Storage(StorageError::Backend)`
//!     with the literal substring `embedding-gemma weights missing`.
//!   - `embedder_is_dyn_compat` — proves the Phase 1 `Embedder` trait stays
//!     dyn-compatible after Phase 2's additions (so `Arc<dyn Embedder>` works
//!     in `Lunaris::with_embedder`).
//!
//! `--features embedder-it` additionally runs:
//!   - `candle_gemma_embeds_real_batch` (requires `LUNARIS_EMBED_GEMMA_PATH` env)
//!   - `ollama_embeds_real_batch` (requires `OLLAMA_URL` env)
//!
//! Both SKIP gracefully when the env var is unset (matches Plan 01-03's
//! `MOON_URL` skip pattern).

use std::sync::Arc;

use lunaris_core::{Embedder, StubEmbedder};

#[cfg(feature = "candle")]
use lunaris_core::{LunarisError, StorageError};
#[cfg(feature = "candle")]
use lunaris_embed::{CandleEmbeddingGemma, CandleEmbeddingGemmaOpts};

#[cfg(all(feature = "ollama", feature = "embedder-it"))]
use lunaris_embed::{OllamaEmbedder, OllamaEmbedderOpts};

#[cfg(feature = "candle")]
#[tokio::test]
async fn missing_weights_returns_actionable_error() {
    let nonexistent = std::path::PathBuf::from(format!(
        "/tmp/lunaris-nonexistent-embedder-cache-{}",
        std::process::id()
    ));
    let opts = CandleEmbeddingGemmaOpts {
        model_path: Some(nonexistent.clone()),
        device: candle_core::Device::Cpu,
    };
    let err = CandleEmbeddingGemma::new(opts).await.expect_err("missing path must error");
    match err {
        LunarisError::Storage(StorageError::Backend(msg)) => {
            assert!(
                msg.contains("embedding-gemma weights missing"),
                "actionable error must contain literal hint, got: {msg}"
            );
            assert!(
                msg.contains(&nonexistent.to_string_lossy().to_string()),
                "error should include the offending path so the operator can fix it: {msg}"
            );
        }
        other => panic!("expected Storage(Backend), got {other:?}"),
    }
}

#[test]
fn embedder_is_dyn_compat() {
    // Compile-time proof that the trait is object-safe — `Arc<dyn Embedder>` is
    // what `Lunaris::with_embedder` accepts. If a Phase 2 addition (CandleEmbeddingGemma
    // / OllamaEmbedder methods) accidentally added a generic method or `Self: Sized`
    // bound, this test stops compiling.
    fn _accepts_dyn(_: &dyn Embedder) {}
    let stub: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    _accepts_dyn(stub.as_ref());
    assert_eq!(stub.dim(), 768);
}

#[cfg(all(feature = "candle", feature = "embedder-it"))]
#[tokio::test]
async fn candle_gemma_embeds_real_batch() {
    let Some(path) = std::env::var("LUNARIS_EMBED_GEMMA_PATH").ok() else {
        eprintln!("SKIP candle_gemma_embeds_real_batch — set LUNARIS_EMBED_GEMMA_PATH to a directory containing tokenizer.json + model.safetensors");
        return;
    };
    let embedder = CandleEmbeddingGemma::new(CandleEmbeddingGemmaOpts {
        model_path: Some(std::path::PathBuf::from(path)),
        device: candle_core::Device::Cpu,
    })
    .await
    .expect("real EmbeddingGemma should load");
    let inputs = ["hello", "world", "rust async is fun", "lunaris memory engine", "vector search"];
    let inputs_ref: Vec<&str> = inputs.iter().copied().collect();
    let out = embedder.embed_batch(&inputs_ref).await.expect("real embed");
    assert_eq!(out.len(), 5);
    for (i, row) in out.iter().enumerate() {
        assert_eq!(row.len(), 768, "row {i} must be 768-d");
        let l2 = row.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
        assert!(
            (0.99..=1.01).contains(&l2),
            "row {i} L2 = {l2} not in [0.99, 1.01]"
        );
    }
}

#[cfg(all(feature = "ollama", feature = "embedder-it"))]
#[tokio::test]
async fn ollama_embeds_real_batch() {
    let Some(url) = std::env::var("OLLAMA_URL").ok() else {
        eprintln!("SKIP ollama_embeds_real_batch — set OLLAMA_URL (e.g. http://localhost:11434)");
        return;
    };
    let embedder = OllamaEmbedder::new(OllamaEmbedderOpts {
        endpoint: url,
        model: std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "embeddinggemma".to_string()),
        dim: 768,
    })
    .expect("client builds");
    let out = embedder.embed_batch(&["hello", "world"]).await.expect("ollama embed");
    assert_eq!(out.len(), 2);
    for row in &out {
        assert_eq!(row.len(), 768, "Ollama returned wrong-shape row");
    }
}
