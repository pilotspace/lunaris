//! Extractor contract tests for Phase 3 graph-pipeline scaffold.
//!
//! Default features (`candle`) run:
//!   - `extractor_is_dyn_compat` — `Arc<dyn Extractor>` constructible.
//!   - `noop_extractor_works` — empty in / empty out per the
//!     [`crate::Extractor::applies`] short-circuit invariant.
//!   - `missing_weights_returns_actionable_error` — load-bearing assertion
//!     for the cache-miss → actionable-error contract (Plan 02-03 pattern):
//!     verifies the error string contains both `gemma-3-4b-it weights
//!     missing` AND `huggingface-cli download` AND the offending path so
//!     the operator can copy-paste the fix command.
//!
//! `--features extractor-it` additionally runs:
//!   - `candle_extracts_real_batch` (requires `LUNARIS_EXTRACT_GEMMA_PATH`
//!     env pointing at a directory containing tokenizer.json + config.json
//!     + model.safetensors)
//!   - `ollama_extracts_real_batch` (requires `OLLAMA_URL`)
//!   - `cloud_api_extracts_real_batch` (requires
//!     `LUNARIS_EXTRACT_PROVIDER` + `<PROVIDER>_API_KEY`)
//!
//! All gated tests SKIP cleanly via `eprintln!("SKIP …")` and early return
//! when the env var is unset (matches Plan 02-01 + Plan 02-03's
//! `LUNARIS_EMBED_GEMMA_PATH` / `LUNARIS_RERANK_BGE_PATH` skip discipline).

use std::sync::Arc;

use lunaris_extract::{ChunkInput, Extractor, NoopExtractor};
use ulid::Ulid;

#[cfg(feature = "candle")]
use lunaris_core::{LunarisError, StorageError};
#[cfg(feature = "candle")]
use lunaris_extract::{CandleGemma3_4B, CandleGemma3_4BOpts};

#[cfg(feature = "ollama")]
#[allow(unused_imports)]
use lunaris_extract::{OllamaExtractor, OllamaExtractorOpts};

#[cfg(feature = "cloud-api")]
use lunaris_extract::{CloudApiExtractor, CloudApiExtractorOpts, CloudProvider};

/// Compile-time + runtime proof that the trait stays object-safe — `Arc<dyn
/// Extractor>` is what `Lunaris::with_extractor` (Plan 03-03) accepts. If a
/// later trait addition (generic method, `Self: Sized` bound) breaks this,
/// the umbrella handle's slot stops compiling.
#[test]
fn extractor_is_dyn_compat() {
    fn _accepts_dyn(_: &dyn Extractor) {}
    let noop: Arc<dyn Extractor> = Arc::new(NoopExtractor);
    _accepts_dyn(noop.as_ref());
    assert!(!noop.applies(), "NoopExtractor::applies must be false");
}

/// `NoopExtractor` produces one empty `RawExtraction` per input chunk so the
/// per-chunk index alignment downstream still holds. Plan 03-03's ingest
/// fan-out skips the GraphNode/GraphEdge WriteOp loop when
/// `extractor.applies() == false` so the empties never even reach storage.
#[tokio::test]
async fn noop_extractor_works() {
    let chunks = vec![
        ChunkInput { chunk_id: Ulid::new(), text: "alpha".into(), heading_path: vec![] },
        ChunkInput {
            chunk_id: Ulid::new(),
            text: "beta gamma".into(),
            heading_path: vec!["intro".into()],
        },
    ];
    let extracted = NoopExtractor.extract(Ulid::new(), &chunks).await.unwrap();
    assert_eq!(extracted.by_chunk.len(), 2);
    assert_eq!(extracted.by_chunk[0].source_chunk_id, chunks[0].chunk_id);
    assert_eq!(extracted.by_chunk[1].source_chunk_id, chunks[1].chunk_id);
    for r in &extracted.by_chunk {
        assert!(r.entities.is_empty());
        assert!(r.relations.is_empty());
        assert!(r.facts.is_empty());
    }
}

/// Load-bearing test for the cache-miss contract (Plan 02-03 pattern).
///
/// The umbrella `Lunaris::open(url)` (Plan 03-03) catches this error and
/// substitutes [`NoopExtractor`] with `tracing::warn!`, so the dev box
/// without a 4B model on disk still runs. The error MUST include all three:
/// `gemma-3-4b-it weights missing`, `huggingface-cli download`, AND the
/// offending path — so an operator who sees the warn line can copy-paste
/// the exact command to download the cache.
#[cfg(feature = "candle")]
#[tokio::test]
async fn missing_weights_returns_actionable_error() {
    let nonexistent = std::path::PathBuf::from(format!(
        "/tmp/lunaris-nonexistent-extractor-cache-{}",
        std::process::id()
    ));
    let opts = CandleGemma3_4BOpts {
        model_path: Some(nonexistent.clone()),
        device: candle_core::Device::Cpu,
        batch_timeout_ms: 150,
        per_chunk_timeout_ms: 450,
        max_new_tokens: 512,
    };
    let err = CandleGemma3_4B::new(opts).await.expect_err("missing path must error");
    match err {
        LunarisError::Storage(StorageError::Backend(msg)) => {
            assert!(
                msg.contains("gemma-3-4b-it weights missing"),
                "actionable error must contain literal hint, got: {msg}"
            );
            assert!(
                msg.contains("huggingface-cli download"),
                "actionable error must include install hint, got: {msg}"
            );
            assert!(
                msg.contains(&nonexistent.to_string_lossy().to_string()),
                "actionable error must include the offending path, got: {msg}"
            );
        }
        other => panic!("expected Storage(Backend), got {other:?}"),
    }
}

/// Real Gemma-3 4B forward-pass (extractor-it gated, env-gated).
///
/// SKIPs cleanly when `LUNARIS_EXTRACT_GEMMA_PATH` is unset so CI default
/// `cargo test --features extractor-it` doesn't fail without the model
/// cached. When the env is set, runs ONE small batch through the real
/// candle stack and asserts the result has one entry per input chunk.
#[cfg(all(feature = "candle", feature = "extractor-it"))]
#[tokio::test]
async fn candle_extracts_real_batch() {
    let Some(path) = std::env::var("LUNARIS_EXTRACT_GEMMA_PATH").ok() else {
        eprintln!(
            "SKIP candle_extracts_real_batch — set LUNARIS_EXTRACT_GEMMA_PATH to a directory containing tokenizer.json + config.json + model.safetensors"
        );
        return;
    };
    let extractor = CandleGemma3_4B::new(CandleGemma3_4BOpts {
        model_path: Some(std::path::PathBuf::from(path)),
        device: candle_core::Device::Cpu,
        // generous timeouts for the live test (real forward pass on CPU is slow)
        batch_timeout_ms: 30_000,
        per_chunk_timeout_ms: 30_000,
        max_new_tokens: 256,
    })
    .await
    .expect("real Gemma-3 4B should load");

    let chunks = vec![ChunkInput {
        chunk_id: Ulid::new(),
        text: "Alice Smith was born in Paris in 1990.".into(),
        heading_path: vec!["bio".into()],
    }];
    let out = extractor.extract(Ulid::new(), &chunks).await.expect("real extract");
    assert_eq!(out.by_chunk.len(), 1);
    assert_eq!(out.by_chunk[0].source_chunk_id, chunks[0].chunk_id);
}

/// Live Ollama extractor (extractor-it gated, env-gated).
#[cfg(all(feature = "ollama", feature = "extractor-it"))]
#[tokio::test]
async fn ollama_extracts_real_batch() {
    let Some(url) = std::env::var("OLLAMA_URL").ok() else {
        eprintln!("SKIP ollama_extracts_real_batch — set OLLAMA_URL (e.g. http://localhost:11434)");
        return;
    };
    let extractor = OllamaExtractor::new(OllamaExtractorOpts {
        endpoint: url,
        model: std::env::var("OLLAMA_EXTRACT_MODEL").unwrap_or_else(|_| "gemma3:4b".to_string()),
        batch_timeout_ms: 30_000,
    })
    .expect("client builds");
    let chunks = vec![ChunkInput {
        chunk_id: Ulid::new(),
        text: "Alice Smith was born in Paris in 1990.".into(),
        heading_path: vec!["bio".into()],
    }];
    let out = extractor.extract(Ulid::new(), &chunks).await.expect("ollama extract");
    assert_eq!(out.by_chunk.len(), 1);
}

/// Live cloud-API extractor (extractor-it gated, env-gated).
#[cfg(all(feature = "cloud-api", feature = "extractor-it"))]
#[tokio::test]
async fn cloud_api_extracts_real_batch() {
    let Some(provider_str) = std::env::var("LUNARIS_EXTRACT_PROVIDER").ok() else {
        eprintln!(
            "SKIP cloud_api_extracts_real_batch — set LUNARIS_EXTRACT_PROVIDER (anthropic|openai|gemini) and the matching <PROVIDER>_API_KEY env"
        );
        return;
    };
    use std::str::FromStr;
    let provider = CloudProvider::from_str(&provider_str).expect("valid provider");
    let api_key_env = match provider {
        CloudProvider::Anthropic => "ANTHROPIC_API_KEY",
        CloudProvider::OpenAI => "OPENAI_API_KEY",
        CloudProvider::Gemini => "GEMINI_API_KEY",
    };
    let Some(api_key) = std::env::var(api_key_env).ok() else {
        eprintln!("SKIP cloud_api_extracts_real_batch — set {api_key_env}");
        return;
    };
    let extractor = CloudApiExtractor::new(CloudApiExtractorOpts {
        provider,
        model: match provider {
            CloudProvider::Anthropic => "claude-3-5-haiku-latest".into(),
            CloudProvider::OpenAI => "gpt-4o-mini".into(),
            CloudProvider::Gemini => "gemini-2.5-flash".into(),
        },
        api_key,
        batch_timeout_ms: 30_000,
        max_retries: 1,
    })
    .expect("client builds");
    let chunks = vec![ChunkInput {
        chunk_id: Ulid::new(),
        text: "Alice Smith was born in Paris in 1990.".into(),
        heading_path: vec!["bio".into()],
    }];
    let out = extractor.extract(Ulid::new(), &chunks).await.expect("cloud-api extract");
    assert_eq!(out.by_chunk.len(), 1);
}
