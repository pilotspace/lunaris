//! Extractor contract tests for Phase 3 graph-pipeline scaffold.
//!
//! Default features run:
//!   - `extractor_is_dyn_compat` — `Arc<dyn Extractor>` constructible.
//!   - `noop_extractor_works` — empty in / empty out per the
//!     [`crate::Extractor::applies`] short-circuit invariant.
//!
//! `--features extractor-it` additionally runs:
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

#[cfg(feature = "ollama")]
#[allow(unused_imports)]
use lunaris_extract::{OllamaExtractor, OllamaExtractorOpts};

#[cfg(all(feature = "cloud-api", feature = "extractor-it"))]
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
        ChunkInput {
            chunk_id: Ulid::new(),
            text: "alpha".into(),
            heading_path: vec![],
            reference_time_iso: None,
        },
        ChunkInput {
            chunk_id: Ulid::new(),
            text: "beta gamma".into(),
            heading_path: vec!["intro".into()],
            reference_time_iso: None,
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
        timeout_ms: 30_000,
    })
    .expect("client builds");
    let chunks = vec![ChunkInput {
        chunk_id: Ulid::new(),
        text: "Alice Smith was born in Paris in 1990.".into(),
        heading_path: vec!["bio".into()],
        reference_time_iso: None,
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
            "SKIP cloud_api_extracts_real_batch — set LUNARIS_EXTRACT_PROVIDER (anthropic|openai|gemini|minimax) and the matching <PROVIDER>_API_KEY env"
        );
        return;
    };
    use std::str::FromStr;
    let provider = CloudProvider::from_str(&provider_str).expect("valid provider");
    let api_key_env = match provider {
        CloudProvider::Anthropic => "ANTHROPIC_API_KEY",
        CloudProvider::OpenAI => "OPENAI_API_KEY",
        CloudProvider::Gemini => "GEMINI_API_KEY",
        CloudProvider::MiniMax => "MINIMAX_API_KEY",
        // Keyless local servers are the norm — the key stays optional.
        CloudProvider::OpenAiCompat => "LUNARIS_OPENAI_COMPAT_API_KEY",
    };
    let api_key = match std::env::var(api_key_env) {
        Ok(k) => k,
        Err(_) if provider == CloudProvider::OpenAiCompat => String::new(),
        Err(_) => {
            eprintln!("SKIP cloud_api_extracts_real_batch — set {api_key_env}");
            return;
        }
    };
    let extractor = CloudApiExtractor::new(CloudApiExtractorOpts {
        provider,
        model: match provider {
            CloudProvider::Anthropic => "claude-3-5-haiku-latest".into(),
            CloudProvider::OpenAI => "gpt-4o-mini".into(),
            CloudProvider::Gemini => "gemini-2.5-flash".into(),
            CloudProvider::MiniMax => "MiniMax-M3".into(),
            CloudProvider::OpenAiCompat => {
                std::env::var("OPENAI_COMPAT_EXTRACT_MODEL").unwrap_or_default()
            }
        },
        api_key,
        batch_timeout_ms: 30_000,
        max_retries: 1,
        max_tokens: 512,
        concurrency: 4,
        base_url: std::env::var("LUNARIS_OPENAI_COMPAT_BASE_URL").ok(),
    })
    .expect("client builds");
    let chunks = vec![ChunkInput {
        chunk_id: Ulid::new(),
        text: "Alice Smith was born in Paris in 1990.".into(),
        heading_path: vec!["bio".into()],
        reference_time_iso: None,
    }];
    let out = extractor.extract(Ulid::new(), &chunks).await.expect("cloud-api extract");
    assert_eq!(out.by_chunk.len(), 1);
}
