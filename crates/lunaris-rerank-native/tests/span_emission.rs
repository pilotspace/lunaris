//! Span-emission test for the §4b "inference microscope" instrumentation
//! (`docs/design/quantized-inference-extractor-reranker.md`).
//!
//! **Red before green:** before the `lunaris.rerank.{tokenize_pairs,forward,
//! score_extract,copy}` spans were added to `reranker.rs`/`xlmr_reranker.rs`/
//! `quantized_reranker.rs`/`quantized_xlmr.rs`, this test failed with
//! "expected span `lunaris.rerank.forward` was never emitted". It is green
//! now that the spans are wired.
//!
//! Gated behind `reranker-gguf` + real GGUF weights: this host has NO
//! `model.safetensors` staged for bge-reranker-v2-m3 (only `tokenizer.json` +
//! `config.json`), so the FP32 path cannot construct a real `NativeReranker`
//! here. The Q5_K_M-imatrix GGUF IS staged
//! (`~/.lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf`), so
//! `NativeQuantizedReranker` is what actually proves the spans fire on this
//! host — CPU device, K=2, well inside the "short debug-mode smoke test"
//! ceiling the live-benchmark constraint allows (no sustained Metal, no
//! `lunaris-bench`).
//!
//! Skips (does not fail) when the artifacts are missing, mirroring the
//! `reranker-it` / `quantized_equivalence.rs` convention in this crate.

#![cfg(feature = "reranker-gguf")]

mod support;

use std::path::PathBuf;

use candle_core::Device;
use lunaris_rerank_native::{NativeQuantizedReranker, NativeQuantizedRerankerOpts};
use support::CapturingLayer;
use tracing_subscriber::layer::SubscriberExt;

/// Resolve a path from an env var first (matches the `BGE_RERANKER_*_PATH`
/// convention used by `tests/quantized_equivalence.rs`), falling back to the
/// well-known on-disk cache layout documented in
/// `docs/design/quantized-inference-extractor-reranker.md` §"Facts" so this
/// test actually runs (not just skips) on hosts with the standard cache.
fn resolve_path(env_var: &str, fallback: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(env_var).map(PathBuf::from) {
        return Some(p);
    }
    fallback.filter(|p| p.exists())
}

fn default_cache_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".cache/lunaris/models/bge-reranker-v2-m3"))
}

fn default_gguf_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf"))
}

#[test]
fn quantized_rerank_emits_the_four_microscope_spans() {
    let cache_dir = default_cache_dir();
    let tokenizer_path = resolve_path(
        "BGE_RERANKER_TOKENIZER_PATH",
        cache_dir.clone().map(|d| d.join("tokenizer.json")),
    );
    let config_path =
        resolve_path("BGE_RERANKER_CONFIG_PATH", cache_dir.map(|d| d.join("config.json")));
    let gguf_path = resolve_path("BGE_RERANKER_GGUF_PATH", default_gguf_path());

    let (Some(tokenizer_path), Some(config_path), Some(gguf_path)) =
        (tokenizer_path, config_path, gguf_path)
    else {
        eprintln!(
            "[skip] quantized_rerank_emits_the_four_microscope_spans — bge-reranker GGUF/tokenizer/config \
             not found (set BGE_RERANKER_TOKENIZER_PATH/BGE_RERANKER_CONFIG_PATH/BGE_RERANKER_GGUF_PATH)"
        );
        return;
    };

    let layer = CapturingLayer::new();
    let subscriber = tracing_subscriber::registry().with(layer.clone());

    // `with_default` scopes the subscriber to the CURRENT thread for the
    // duration of the closure. We deliberately call the synchronous
    // `score_blocking` path here (NOT the `Reranker::rerank` trait method,
    // which dispatches onto a `spawn_blocking` thread-pool thread — a
    // different OS thread than the one `with_default` installed the
    // thread-local subscriber on, so those spans would silently vanish from
    // this capture). `score_blocking` runs the exact same instrumented code
    // path (`tokenize_pairs` → `forward`/`score_extract` inside
    // `QuantizedXlmRoberta::score` → `copy`) on this thread, which is what
    // the production `rerank` wrapper calls after crossing the
    // `spawn_blocking` boundary.
    let scores_by_name = tracing::subscriber::with_default(subscriber, || {
        let reranker = NativeQuantizedReranker::open(NativeQuantizedRerankerOpts {
            gguf_path,
            tokenizer_path,
            config_path,
            device: Device::Cpu,
        })
        .expect("open quantized reranker");

        // Two pairs with different doc lengths so max_seq_len / padded_tokens
        // are non-degenerate.
        let pairs = [
            ("what is rust", "a short doc"),
            ("what is rust", "a considerably longer document to pad the batch against"),
        ];
        let _ = reranker.score_blocking(&pairs).expect("score_blocking");

        layer.captured()
    });

    let find = |name: &str| scores_by_name.iter().find(|s| s.name == name);

    let tokenize = find("lunaris.rerank.tokenize_pairs")
        .unwrap_or_else(|| panic!("lunaris.rerank.tokenize_pairs span was never emitted"));
    assert_eq!(tokenize.fields.get("batch_size").map(String::as_str), Some("2"));
    assert!(
        tokenize.fields.contains_key("max_seq_len"),
        "tokenize_pairs span must record max_seq_len post-hoc: {:?}",
        tokenize.fields
    );
    assert!(
        tokenize.fields.contains_key("real_tokens"),
        "tokenize_pairs span must record real_tokens post-hoc: {:?}",
        tokenize.fields
    );
    assert!(
        tokenize.fields.contains_key("padded_tokens"),
        "tokenize_pairs span must record padded_tokens post-hoc: {:?}",
        tokenize.fields
    );

    let forward = find("lunaris.rerank.forward")
        .unwrap_or_else(|| panic!("lunaris.rerank.forward span was never emitted"));
    assert_eq!(forward.fields.get("batch_size").map(String::as_str), Some("2"));

    let score_extract = find("lunaris.rerank.score_extract")
        .unwrap_or_else(|| panic!("lunaris.rerank.score_extract span was never emitted"));
    assert_eq!(score_extract.fields.get("batch_size").map(String::as_str), Some("2"));

    let copy = find("lunaris.rerank.copy")
        .unwrap_or_else(|| panic!("lunaris.rerank.copy span was never emitted"));
    assert_eq!(copy.fields.get("batch_size").map(String::as_str), Some("2"));
}
