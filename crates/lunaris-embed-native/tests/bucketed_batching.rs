//! Wiring proof for length-bucketed batch assembly (§4b-RESULTS finding #1,
//! `docs/design/quantized-inference-extractor-reranker.md`).
//!
//! The 2026-07-10 profiling matrix measured pad-to-longest batching making
//! batch=8 SLOWER than batch=1 in every cell (67% padding waste on the
//! synthetic embed corpus). The fix is in `Embedder::embed_batch`'s batch
//! assembly: sort inputs by byte length, plan windows over the sorted order
//! (row cap + activation budget + a padding-waste ceiling), forward each
//! length-homogeneous window, then scatter rows back to input order.
//!
//! **Red before green:** against the pre-bucketing `embed_batch` (contiguous-window
//! planner), the interleaved short/long corpus below fits one
//! window of 6 → the forward span records `batch_size=6` and the tokenize
//! span records `padded_tokens > 0`. Both asserts fail. After bucketing the
//! plan is two windows of 3 identical-length inputs → `padded_tokens == 0`.
//!
//! Observability: the §4b microscope spans (`lunaris.embed.tokenize` /
//! `lunaris.embed.forward`) double as the wiring probe — window shapes and
//! padding are read from the spans the production hot path already emits.
//! `embed_batch` crosses a `spawn_blocking` boundary, so a thread-local
//! `with_default` subscriber would miss those spans — this file installs a
//! process-global subscriber instead (single `#[test]` per concern, one
//! capture window per phase, indices snapshotted between phases).
//!
//! Skips (does not fail) per-phase when artifacts are missing: the FP32
//! phase needs `model.safetensors` staged; the quantized phase needs the
//! Q4_K_M GGUF and the `embedder-gguf` feature.

mod support;

use std::path::PathBuf;
use std::sync::OnceLock;

use candle_core::Device;
use lunaris_core::Embedder;
use lunaris_embed_native::{NativeEmbedder, NativeEmbedderOpts};
use support::{CapturedSpan, CapturingLayer};
use tracing_subscriber::layer::SubscriberExt;

/// Install the capturing layer as the PROCESS-global subscriber exactly once
/// and hand back the shared layer. Global (not thread-local) because
/// `embed_batch` emits its spans from a `spawn_blocking` pool thread.
/// Serializes the two phases: both share the one global capture layer, so
/// concurrent runs would interleave spans across each other's `[before..]`
/// windows. Async-aware (tokio) mutex — the guard is held across the
/// `embed_batch(...).await` point, and the workspace lock-across-await rule
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
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h).join(".cache/lunaris/models/granite-embedding-311m-multilingual-r2")
    })
}

/// Interleaved corpus: identical short strings at even indices, identical
/// long strings (~1.2 KB) at odd indices. Identical strings per length class
/// make the "zero padding after bucketing" assert exact (equal token counts
/// within a window), and make row equality checks bitwise.
fn interleaved_corpus() -> Vec<String> {
    let short = "a tiny memo about the standup.".to_string();
    let long = "the quarterly architecture review covered the storage engine migration, \
                the bi-temporal fact compaction schedule, and the retrieval fusion weights. "
        .repeat(8);
    vec![short.clone(), long.clone(), short.clone(), long.clone(), short, long]
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

/// Assert the bucketing contract from a capture window + the returned rows.
fn assert_bucketed(spans: &[CapturedSpan], rows: &[Vec<f32>], phase: &str) {
    // Window shapes: two forwards of 3, never one mixed forward of 6.
    let forward_sizes: Vec<&str> = spans
        .iter()
        .filter(|s| s.name == "lunaris.embed.forward")
        .filter_map(|s| s.fields.get("batch_size").map(String::as_str))
        .collect();
    assert_eq!(
        forward_sizes,
        vec!["3", "3"],
        "[{phase}] interleaved short/long corpus must forward as two \
         length-homogeneous windows of 3, got batch sizes {forward_sizes:?}"
    );

    // Padding: identical strings per window ⇒ exactly zero padded tokens.
    let padded: u64 = spans
        .iter()
        .filter(|s| s.name == "lunaris.embed.tokenize")
        .filter_map(|s| s.fields.get("padded_tokens"))
        .filter_map(|v| v.parse::<u64>().ok())
        .sum();
    assert_eq!(
        padded, 0,
        "[{phase}] bucketed windows of identical-length inputs must tokenize \
         with zero padding, got {padded} padded tokens"
    );

    // Scatter-back order: rows land at their input positions.
    assert_eq!(rows.len(), 6, "[{phase}] one row per input");
    for (i, j) in [(0usize, 2usize), (2, 4), (1, 3), (3, 5)] {
        assert_eq!(
            rows[i], rows[j],
            "[{phase}] identical inputs at positions {i}/{j} must produce identical rows"
        );
    }
    let cross = cosine(&rows[0], &rows[1]);
    assert!(
        cross < 0.999,
        "[{phase}] short and long inputs must not collapse to the same row \
         (cross cosine {cross}) — scatter-back would be unproven otherwise"
    );
}

/// FP32 `NativeEmbedder` phase — the default-features production path.
#[tokio::test]
async fn embed_batch_buckets_by_length_and_preserves_order() {
    let _serial = PHASE_SERIAL.lock().await;
    let layer = global_layer();
    let cache_dir = default_cache_dir();
    let tokenizer_path = resolve_path(
        "GRANITE_R2_TOKENIZER_PATH",
        cache_dir.clone().map(|d| d.join("tokenizer.json")),
    );
    let config_path =
        resolve_path("GRANITE_R2_CONFIG_PATH", cache_dir.clone().map(|d| d.join("config.json")));
    let weights_path =
        resolve_path("GRANITE_R2_WEIGHTS_PATH", cache_dir.map(|d| d.join("model.safetensors")));

    let (Some(tokenizer_path), Some(config_path), Some(weights_path)) =
        (tokenizer_path, config_path, weights_path)
    else {
        eprintln!(
            "[skip] embed_batch_buckets_by_length_and_preserves_order — granite-r2 \
             safetensors/tokenizer/config not staged"
        );
        return;
    };

    let embedder = NativeEmbedder::open(NativeEmbedderOpts {
        weights_path,
        tokenizer_path,
        config_path,
        device: Device::Cpu,
    })
    .expect("open FP32 embedder");

    let corpus = interleaved_corpus();
    let refs: Vec<&str> = corpus.iter().map(String::as_str).collect();

    let before = layer.captured().len();
    let rows = embedder.embed_batch(&refs).await.expect("embed_batch");
    let spans = layer.captured()[before..].to_vec();

    assert_bucketed(&spans, &rows, "fp32");
}

/// Q4_K_M `NativeQuantizedEmbedder` phase — the `embedder-gguf` path.
#[cfg(feature = "embedder-gguf")]
#[tokio::test]
async fn quantized_embed_batch_buckets_by_length_and_preserves_order() {
    use lunaris_embed_native::{NativeQuantizedEmbedder, NativeQuantizedEmbedderOpts};

    let _serial = PHASE_SERIAL.lock().await;
    let layer = global_layer();
    let cache_dir = default_cache_dir();
    let tokenizer_path = resolve_path(
        "GRANITE_R2_TOKENIZER_PATH",
        cache_dir.clone().map(|d| d.join("tokenizer.json")),
    );
    let config_path =
        resolve_path("GRANITE_R2_CONFIG_PATH", cache_dir.map(|d| d.join("config.json")));
    let gguf_path = resolve_path(
        "GRANITE_R2_GGUF_PATH",
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join(".lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf")
        }),
    );

    let (Some(tokenizer_path), Some(config_path), Some(gguf_path)) =
        (tokenizer_path, config_path, gguf_path)
    else {
        eprintln!(
            "[skip] quantized_embed_batch_buckets_by_length_and_preserves_order — granite-r2 \
             GGUF/tokenizer/config not staged"
        );
        return;
    };

    let embedder = NativeQuantizedEmbedder::open(NativeQuantizedEmbedderOpts {
        gguf_path,
        tokenizer_path,
        config_path,
        device: Device::Cpu,
    })
    .expect("open quantized embedder");

    let corpus = interleaved_corpus();
    let refs: Vec<&str> = corpus.iter().map(String::as_str).collect();

    let before = layer.captured().len();
    let rows = embedder.embed_batch(&refs).await.expect("embed_batch");
    let spans = layer.captured()[before..].to_vec();

    assert_bucketed(&spans, &rows, "q4_k_m");
}
