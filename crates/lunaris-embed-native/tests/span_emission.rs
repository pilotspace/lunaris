//! Span-emission test for the §4b "inference microscope" instrumentation
//! (`docs/design/quantized-inference-extractor-reranker.md`).
//!
//! **Red before green:** before the `lunaris.embed.{tokenize,forward,pooling,
//! copy}` spans were added to `embedder.rs`/`modernbert.rs`/
//! `quantized_embedder.rs`/`quantized_modernbert.rs`, this test failed with
//! "expected span `lunaris.embed.forward` was never emitted" (the capturing
//! layer's `Vec` came back with only http/tokio-runtime noise, zero
//! `lunaris.embed.*` entries). It is green now that the spans are wired.
//!
//! Gated behind `embedder-gguf` + real GGUF weights: this host has NO
//! `model.safetensors` staged for granite-r2 (only `tokenizer.json` +
//! `config.json` — see the plan doc's baseline caveat), so the FP32 path
//! cannot construct a real `NativeEmbedder` here. The Q4_K_M GGUF IS staged
//! (`~/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf`),
//! so `NativeQuantizedEmbedder` is what actually proves the spans fire on
//! this host — CPU device, batch of 2, well inside the "short debug-mode
//! smoke test" ceiling the live-benchmark constraint allows (no sustained
//! Metal, no `lunaris-bench`).
//!
//! Skips (does not fail) when the artifacts are missing, mirroring the
//! `embedder-it` / `quantized_equivalence.rs` convention in this crate.

#![cfg(feature = "embedder-gguf")]

mod support;

use std::path::PathBuf;

use candle_core::Device;
use lunaris_embed_native::{NativeQuantizedEmbedder, NativeQuantizedEmbedderOpts};
use support::CapturingLayer;
use tracing_subscriber::layer::SubscriberExt;

/// Resolve a path from an env var first (matches the `GRANITE_R2_*_PATH`
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
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h).join(".cache/lunaris/models/granite-embedding-311m-multilingual-r2")
    })
}

fn default_gguf_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf")
    })
}

#[test]
fn quantized_embed_emits_the_four_microscope_spans() {
    let cache_dir = default_cache_dir();
    let tokenizer_path =
        resolve_path("GRANITE_R2_TOKENIZER_PATH", cache_dir.clone().map(|d| d.join("tokenizer.json")));
    let config_path =
        resolve_path("GRANITE_R2_CONFIG_PATH", cache_dir.map(|d| d.join("config.json")));
    let gguf_path = resolve_path("GRANITE_R2_GGUF_PATH", default_gguf_path());

    let (Some(tokenizer_path), Some(config_path), Some(gguf_path)) =
        (tokenizer_path, config_path, gguf_path)
    else {
        eprintln!(
            "[skip] quantized_embed_emits_the_four_microscope_spans — granite-r2 GGUF/tokenizer/config \
             not found (set GRANITE_R2_TOKENIZER_PATH/GRANITE_R2_CONFIG_PATH/GRANITE_R2_GGUF_PATH)"
        );
        return;
    };

    let layer = CapturingLayer::new();
    let subscriber = tracing_subscriber::registry().with(layer.clone());

    // `with_default` scopes the subscriber to the CURRENT thread for the
    // duration of the closure. We deliberately call the synchronous
    // `embed_blocking` path here (NOT the `Embedder::embed_batch` trait
    // method, which dispatches onto a `spawn_blocking` thread-pool thread —
    // a different OS thread than the one `with_default` installed the
    // thread-local subscriber on, so those spans would silently vanish from
    // this capture). `embed_blocking` runs the exact same instrumented code
    // path (`tokenize` → `forward`/`pooling` inside `pooled_forward` →
    // `copy`) on this thread, which is what the production `embed_batch`
    // wrapper calls after crossing the `spawn_blocking` boundary.
    let scores_by_name = tracing::subscriber::with_default(subscriber, || {
        let embedder = NativeQuantizedEmbedder::open(NativeQuantizedEmbedderOpts {
            gguf_path,
            tokenizer_path,
            config_path,
            device: Device::Cpu,
        })
        .expect("open quantized embedder");

        // Two short prompts of different lengths so max_seq_len /
        // padded_tokens are non-degenerate (batch of 1 can't show padding
        // waste at all).
        let inputs = ["hello world", "a considerably longer sentence to pad against"];
        let _ = embedder.embed_blocking(&inputs).expect("embed_blocking");

        layer.captured()
    });

    let find = |name: &str| scores_by_name.iter().find(|s| s.name == name);

    let tokenize = find("lunaris.embed.tokenize")
        .unwrap_or_else(|| panic!("lunaris.embed.tokenize span was never emitted"));
    assert_eq!(tokenize.fields.get("batch_size").map(String::as_str), Some("2"));
    assert!(
        tokenize.fields.contains_key("max_seq_len"),
        "tokenize span must record max_seq_len post-hoc: {:?}",
        tokenize.fields
    );
    assert!(
        tokenize.fields.contains_key("real_tokens"),
        "tokenize span must record real_tokens post-hoc: {:?}",
        tokenize.fields
    );
    assert!(
        tokenize.fields.contains_key("padded_tokens"),
        "tokenize span must record padded_tokens post-hoc: {:?}",
        tokenize.fields
    );

    let forward = find("lunaris.embed.forward")
        .unwrap_or_else(|| panic!("lunaris.embed.forward span was never emitted"));
    assert_eq!(forward.fields.get("batch_size").map(String::as_str), Some("2"));

    let pooling = find("lunaris.embed.pooling")
        .unwrap_or_else(|| panic!("lunaris.embed.pooling span was never emitted"));
    assert_eq!(pooling.fields.get("batch_size").map(String::as_str), Some("2"));

    let copy = find("lunaris.embed.copy")
        .unwrap_or_else(|| panic!("lunaris.embed.copy span was never emitted"));
    assert_eq!(copy.fields.get("batch_size").map(String::as_str), Some("2"));
}
