//! Wiring proof for length-bucketed rerank batch assembly (§4b-RESULTS
//! finding #1, `docs/design/quantized-inference-extractor-reranker.md`) —
//! mirror of `lunaris-embed-native/tests/bucketed_batching.rs`.
//!
//! The 2026-07-10 profiling matrix measured 48% padding waste on batch=8
//! rerank pairs (pad-to-longest across mixed-length docs), making batch=8
//! ~3× slower per pair than batch=1 on Metal. The fix is in
//! `Reranker::rerank`'s batch assembly: sort docs by pair byte length, score
//! length-homogeneous windows, scatter scores back to their docs before the
//! existing sort-by-score-desc.
//!
//! **Red before green:** against the pre-bucketing `rerank` (FP path:
//! contiguous `docs.chunks(8)`; quantized path: one unchunked forward over
//! ALL pairs), the interleaved short/long candidate list below scores as one
//! window of 6 → the forward span records `batch_size=6` and
//! `padded_tokens > 0`. After bucketing: two windows of 3, zero padding.
//!
//! Score↔doc pairing is proven via candidate ids: identical texts must come
//! back with identical scores, and short-doc scores must differ from
//! long-doc scores (the docs differ), regardless of the score-desc output
//! order.
//!
//! Skips (does not fail) per-phase when artifacts are missing.

mod support;

use std::path::PathBuf;
use std::sync::OnceLock;

use candle_core::Device;
use lunaris_rerank::{RerankCandidate, Reranker};
use lunaris_rerank_native::{NativeReranker, NativeRerankerOpts};
use support::{CapturedSpan, CapturingLayer};
use tracing_subscriber::layer::SubscriberExt;

/// Serializes the two phases: both share the one global capture layer.
/// Async-aware (tokio) mutex — the guard is held across the
/// `rerank(...).await` point, and the workspace lock-across-await rule
/// (+ clippy `await_holding_lock`) forbids a `std::sync` guard there.
static PHASE_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn global_layer() -> &'static CapturingLayer {
    static LAYER: OnceLock<CapturingLayer> = OnceLock::new();
    LAYER.get_or_init(|| {
        let layer = CapturingLayer::new();
        let subscriber = tracing_subscriber::registry().with(layer.clone());
        tracing::subscriber::set_global_default(subscriber)
            .expect("bucketed_batching.rs must be the only global-subscriber installer");
        layer
    })
}

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

/// Interleaved candidates: identical short docs at even ids, identical long
/// (~1.2 KB) docs at odd ids. Identical docs per length class make the
/// zero-padding assert exact and the equal-score pairing check robust.
fn interleaved_candidates() -> (String, Vec<RerankCandidate>) {
    let query = "what did the architecture review decide about compaction?".to_string();
    let short = "standup notes.".to_string();
    let long = "the quarterly architecture review covered the storage engine migration, \
                the bi-temporal fact compaction schedule, and the retrieval fusion weights. "
        .repeat(8);
    let cand = |id: u8, text: &str| RerankCandidate {
        id: vec![id],
        text: text.to_string(),
        score: 0.0,
        metadata: serde_json::Value::Null,
    };
    let docs = vec![
        cand(0, &short),
        cand(1, &long),
        cand(2, &short),
        cand(3, &long),
        cand(4, &short),
        cand(5, &long),
    ];
    (query, docs)
}

/// Assert the bucketing contract from a capture window + returned candidates.
fn assert_bucketed(spans: &[CapturedSpan], out: &[RerankCandidate], phase: &str) {
    let forward_sizes: Vec<&str> = spans
        .iter()
        .filter(|s| s.name == "lunaris.rerank.forward")
        .filter_map(|s| s.fields.get("batch_size").map(String::as_str))
        .collect();
    assert_eq!(
        forward_sizes,
        vec!["3", "3"],
        "[{phase}] interleaved short/long candidates must forward as two \
         length-homogeneous windows of 3, got batch sizes {forward_sizes:?}"
    );

    let padded: u64 = spans
        .iter()
        .filter(|s| s.name == "lunaris.rerank.tokenize_pairs")
        .filter_map(|s| s.fields.get("padded_tokens"))
        .filter_map(|v| v.parse::<u64>().ok())
        .sum();
    assert_eq!(
        padded, 0,
        "[{phase}] bucketed windows of identical-length pairs must tokenize \
         with zero padding, got {padded} padded tokens"
    );

    // Score↔doc pairing survives the scatter + sort-by-score-desc.
    assert_eq!(out.len(), 6, "[{phase}] rerank must return every candidate");
    let score_of = |id: u8| {
        out.iter()
            .find(|c| c.id == vec![id])
            .unwrap_or_else(|| panic!("[{phase}] candidate id {id} missing from output"))
            .score
    };
    for (a, b) in [(0u8, 2u8), (2, 4), (1, 3), (3, 5)] {
        assert_eq!(
            score_of(a),
            score_of(b),
            "[{phase}] identical docs (ids {a}/{b}) must score identically"
        );
    }
    assert_ne!(
        score_of(0),
        score_of(1),
        "[{phase}] short and long docs must not score identically — \
         score↔doc pairing would be unproven otherwise"
    );
    let mut scores: Vec<f32> = out.iter().map(|c| c.score).collect();
    let mut sorted = scores.clone();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    assert_eq!(scores, sorted, "[{phase}] output must stay sorted by score desc");
    scores.dedup();
}

/// FP32 `NativeReranker` phase — the default-features production path.
#[tokio::test]
async fn rerank_buckets_by_length_and_preserves_score_pairing() {
    let _serial = PHASE_SERIAL.lock().await;
    let layer = global_layer();
    let cache_dir = default_cache_dir();
    let tokenizer_path = resolve_path(
        "BGE_RERANKER_TOKENIZER_PATH",
        cache_dir.clone().map(|d| d.join("tokenizer.json")),
    );
    let config_path =
        resolve_path("BGE_RERANKER_CONFIG_PATH", cache_dir.clone().map(|d| d.join("config.json")));
    let weights_path =
        resolve_path("BGE_RERANKER_WEIGHTS_PATH", cache_dir.map(|d| d.join("model.safetensors")));

    let (Some(tokenizer_path), Some(config_path), Some(weights_path)) =
        (tokenizer_path, config_path, weights_path)
    else {
        eprintln!(
            "[skip] rerank_buckets_by_length_and_preserves_score_pairing — bge-reranker \
             safetensors/tokenizer/config not staged"
        );
        return;
    };

    let reranker = NativeReranker::open(NativeRerankerOpts {
        weights_path,
        tokenizer_path,
        config_path,
        device: Device::Cpu,
    })
    .expect("open FP32 reranker");

    let (query, docs) = interleaved_candidates();
    let before = layer.captured().len();
    let out = reranker.rerank(&query, docs).await.expect("rerank");
    let spans = layer.captured()[before..].to_vec();

    assert_bucketed(&spans, &out, "fp32");
}

/// Q5_K_M `NativeQuantizedReranker` phase — the `reranker-gguf` path.
#[cfg(feature = "reranker-gguf")]
#[tokio::test]
async fn quantized_rerank_buckets_by_length_and_preserves_score_pairing() {
    use lunaris_rerank_native::{NativeQuantizedReranker, NativeQuantizedRerankerOpts};

    let _serial = PHASE_SERIAL.lock().await;
    let layer = global_layer();
    let cache_dir = default_cache_dir();
    let tokenizer_path = resolve_path(
        "BGE_RERANKER_TOKENIZER_PATH",
        cache_dir.clone().map(|d| d.join("tokenizer.json")),
    );
    let config_path =
        resolve_path("BGE_RERANKER_CONFIG_PATH", cache_dir.map(|d| d.join("config.json")));
    let gguf_path = resolve_path(
        "BGE_RERANKER_GGUF_PATH",
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf")),
    );

    let (Some(tokenizer_path), Some(config_path), Some(gguf_path)) =
        (tokenizer_path, config_path, gguf_path)
    else {
        eprintln!(
            "[skip] quantized_rerank_buckets_by_length_and_preserves_score_pairing — \
             bge-reranker GGUF/tokenizer/config not staged"
        );
        return;
    };

    let reranker = NativeQuantizedReranker::open(NativeQuantizedRerankerOpts {
        gguf_path,
        tokenizer_path,
        config_path,
        device: Device::Cpu,
    })
    .expect("open quantized reranker");

    let (query, docs) = interleaved_candidates();
    let before = layer.captured().len();
    let out = reranker.rerank(&query, docs).await.expect("rerank");
    let spans = layer.captured()[before..].to_vec();

    assert_bucketed(&spans, &out, "q5_k_m");
}
