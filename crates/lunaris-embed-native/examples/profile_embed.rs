//! `profile_embed` — corpus-replay profiler for the granite-r2 embedder hot
//! path. Attributes p50/p90/p99 latency to the `lunaris.embed.{tokenize,
//! forward,pooling,copy}` stages instrumented in `embedder.rs`/
//! `modernbert.rs`/`quantized_embedder.rs`/`quantized_modernbert.rs`, plus
//! tokens/s (forward-stage throughput) and the padding-waste ratio
//! (`padded_tokens / real_tokens`, recorded on the `tokenize` span).
//!
//! This is Workstream C of
//! `docs/design/quantized-inference-extractor-reranker.md` §4b: "is the
//! embedder/reranker slow because of candle's matmul kernels, or because of
//! tokenize/batch/copy overhead?" — a measurement, not a guess. The decision
//! rule (§4b): if ≥60% of p50 lives inside `forward` → the candle-vs-
//! llama.cpp runtime swap (§4c) is the lever; if it's tokenize/batch/copy →
//! fix in place and keep candle.
//!
//! ## Usage
//!
//! ```bash
//! # FP32 path (needs LUNARIS_EMBEDDER_DIR/model.safetensors on disk):
//! LUNARIS_EMBEDDER_DIR=~/.cache/lunaris/models/granite-embedding-311m-multilingual-r2 \
//!   cargo run -p lunaris-embed-native --example profile_embed -- \
//!     --device cpu --batch 8 --corpus 64
//!
//! # Q4_K_M GGUF path (needs the `embedder-gguf` feature + a staged GGUF):
//! LUNARIS_EMBEDDER_DIR=~/.cache/lunaris/models/granite-embedding-311m-multilingual-r2 \
//! LUNARIS_EMBEDDER_GGUF=~/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf \
//!   cargo run -p lunaris-embed-native --features embedder-gguf --example profile_embed -- \
//!     --device cpu --batch 8 --corpus 64 --quant gguf
//!
//! # Metal (release build recommended; requires the `metal` feature):
//!   cargo run --release -p lunaris-embed-native --features metal \
//!     --example profile_embed -- --device metal --batch 8 --corpus 64
//! ```
//!
//! ## Env vars (mirror the production resolver — see
//! `crates/lunaris/src/handle.rs`'s `resolve_embedder`)
//!
//! - `LUNARIS_EMBEDDER_DIR` — directory containing `model.safetensors`
//!   (FP32 path), `tokenizer.json`, `config.json`. REQUIRED (tokenizer +
//!   config always come from here, even in `gguf` quant mode).
//! - `LUNARIS_EMBEDDER_GGUF` — path to the Q4_K_M GGUF; only consulted when
//!   `--quant gguf` is passed (or auto-selected because the env var is set
//!   and `--quant` was omitted).
//!
//! ## Flags
//!
//! - `--device cpu|metal` (default `cpu`)
//! - `--batch N` (default 8) — forward-pass batch size (chunks the corpus)
//! - `--corpus N` (default 64) — number of synthetic documents to replay
//! - `--quant fp|gguf` (default: auto — `gguf` iff `LUNARIS_EMBEDDER_GGUF`
//!   is set AND the crate was built with `--features embedder-gguf`)

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use candle_core::Device;
use lunaris_embed_native::{NativeEmbedder, NativeEmbedderOpts};
#[cfg(feature = "embedder-gguf")]
use lunaris_embed_native::{NativeQuantizedEmbedder, NativeQuantizedEmbedderOpts};
use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

// ── deterministic variable-length synthetic corpus ─────────────────────────

/// Small fixed vocabulary — enough lexical variety to avoid the tokenizer's
/// BPE merges collapsing every document to the same handful of ids, without
/// pulling in a `lorem-ipsum`-style dependency.
const WORD_BANK: &[&str] = &[
    "the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "async", "runtime",
    "candle", "tensor", "forward", "pass", "quantized", "matmul", "kernel", "padding", "batch",
    "token", "embedding", "granite", "reranker", "cross", "encoder", "attention", "layer",
    "normalize", "cosine", "similarity", "graph", "memory", "agent", "recall", "scope", "moon",
    "storage", "vector", "index", "search", "query", "document", "chunk", "summary", "entity",
    "relation", "fact", "community", "hybrid", "fusion",
];

/// SplitMix64 — a tiny, allocation-free, deterministic PRNG. Not
/// cryptographic; purely here so `synth_corpus` is 100% reproducible run to
/// run without adding a `rand` dependency to a crate whose Cargo.toml
/// workspace policy is "no new transitive surface for a unit-test/example
/// concern".
fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Build `n` synthetic documents with REALISTIC length variance (mix of
/// 16..512-token-ish texts). Variable lengths are what expose padding waste
/// in a `PaddingStrategy::BatchLongest` tokenizer — a batch of one 500-word
/// RAPTOR-style summary next to nine 20-word chunks pads every short row out
/// to the long row's width. Bucket distribution approximates a real recall
/// corpus: mostly short chunks, occasional medium documents, rare long
/// summaries.
fn synth_corpus(n: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let (lo, hi) = match i % 10 {
            0..=3 => (16, 32),   // 40% short chunks
            4..=6 => (64, 128),  // 30% medium documents
            7..=8 => (200, 350), // 20% long documents
            _ => (400, 512),     // 10% RAPTOR-style summaries
        };
        let span = (hi - lo + 1) as u64;
        let word_count = lo + (splitmix64(i as u64) % span) as usize;
        let words: Vec<&str> = (0..word_count)
            .map(|w| {
                let idx = splitmix64((i as u64) * 7919 + w as u64) as usize % WORD_BANK.len();
                WORD_BANK[idx]
            })
            .collect();
        out.push(words.join(" "));
    }
    out
}

// ── per-stage timing layer ─────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct SpanFields {
    batch_size: Option<u64>,
    real_tokens: Option<u64>,
    padded_tokens: Option<u64>,
}

struct FieldVisitor<'a>(&'a mut SpanFields);

impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}

    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "batch_size" => self.0.batch_size = Some(value),
            "real_tokens" => self.0.real_tokens = Some(value),
            "padded_tokens" => self.0.padded_tokens = Some(value),
            _ => {}
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if let Ok(v) = u64::try_from(value) {
            self.record_u64(field, v);
        }
    }
}

struct SpanState {
    start: Instant,
    fields: SpanFields,
}

#[derive(Debug, Clone)]
struct StageSample {
    name: &'static str,
    duration: Duration,
    fields: SpanFields,
}

#[derive(Clone, Default)]
struct StageTimingLayer {
    samples: Arc<Mutex<Vec<StageSample>>>,
}

impl StageTimingLayer {
    fn new() -> Self {
        Self::default()
    }

    fn samples(&self) -> Vec<StageSample> {
        self.samples.lock().expect("timing layer mutex poisoned").clone()
    }
}

impl<S> Layer<S> for StageTimingLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let mut fields = SpanFields::default();
        attrs.record(&mut FieldVisitor(&mut fields));
        if let Some(span_ref) = ctx.span(id) {
            span_ref.extensions_mut().insert(SpanState { start: Instant::now(), fields });
        }
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span_ref) = ctx.span(id) else { return };
        let mut extensions = span_ref.extensions_mut();
        let Some(state) = extensions.get_mut::<SpanState>() else { return };
        values.record(&mut FieldVisitor(&mut state.fields));
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let Some(span_ref) = ctx.span(&id) else { return };
        let name = span_ref.name();
        let extensions = span_ref.extensions();
        let Some(state) = extensions.get::<SpanState>() else { return };
        let duration = state.start.elapsed();
        let fields = state.fields.clone();
        drop(extensions);
        self.samples.lock().expect("timing layer mutex poisoned").push(StageSample {
            name,
            duration,
            fields,
        });
    }
}

// ── reporting ────────────────────────────────────────────────────────────

fn percentile(sorted_ns: &[u128], p: f64) -> f64 {
    if sorted_ns.is_empty() {
        return 0.0;
    }
    let idx = (((sorted_ns.len() - 1) as f64) * p).round() as usize;
    sorted_ns[idx.min(sorted_ns.len() - 1)] as f64 / 1_000_000.0
}

fn print_report(samples: &[StageSample], label: &str) {
    let mut by_stage: BTreeMap<&'static str, Vec<u128>> = BTreeMap::new();
    let mut real_tokens_total: u64 = 0;
    let mut padded_tokens_total: u64 = 0;
    let mut forward_total_ns: u128 = 0;

    for s in samples {
        by_stage.entry(s.name).or_default().push(s.duration.as_nanos());
        if let Some(rt) = s.fields.real_tokens {
            real_tokens_total += rt;
        }
        if let Some(pt) = s.fields.padded_tokens {
            padded_tokens_total += pt;
        }
        if s.name.ends_with(".forward") {
            forward_total_ns += s.duration.as_nanos();
        }
    }

    println!("\n=== {label} — per-stage latency (ms) ===");
    println!("{:<28} {:>5} {:>9} {:>9} {:>9}", "stage", "n", "p50", "p90", "p99");
    for (name, mut durs) in by_stage {
        durs.sort_unstable();
        println!(
            "{:<28} {:>5} {:>9.3} {:>9.3} {:>9.3}",
            name,
            durs.len(),
            percentile(&durs, 0.50),
            percentile(&durs, 0.90),
            percentile(&durs, 0.99),
        );
    }

    let total_tokens = real_tokens_total + padded_tokens_total;
    let waste_pct =
        if total_tokens > 0 { 100.0 * padded_tokens_total as f64 / total_tokens as f64 } else { 0.0 };
    let forward_s = forward_total_ns as f64 / 1_000_000_000.0;
    let tokens_per_sec = if forward_s > 0.0 { real_tokens_total as f64 / forward_s } else { 0.0 };

    println!(
        "\nreal_tokens={real_tokens_total} padded_tokens={padded_tokens_total} \
         padding_waste={waste_pct:.1}% forward_tokens_per_sec={tokens_per_sec:.1}"
    );
}

// ── CLI ──────────────────────────────────────────────────────────────────

struct Args {
    device: String,
    batch: usize,
    corpus: usize,
    quant: Option<String>,
}

fn parse_args() -> Args {
    let mut device = "cpu".to_string();
    let mut batch = 8usize;
    let mut corpus = 64usize;
    let mut quant = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--device" => device = it.next().unwrap_or_else(|| "cpu".into()),
            "--batch" => batch = it.next().and_then(|v| v.parse().ok()).unwrap_or(8),
            "--corpus" => corpus = it.next().and_then(|v| v.parse().ok()).unwrap_or(64),
            "--quant" => quant = it.next(),
            other => eprintln!("[profile_embed] ignoring unknown arg: {other}"),
        }
    }
    Args { device, batch, corpus, quant }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn resolve_device(name: &str) -> Device {
    match name {
        "cpu" => Device::Cpu,
        "metal" => Device::new_metal(0).unwrap_or_else(|e| {
            panic!(
                "--device metal requested but Device::new_metal(0) failed: {e}. \
                 Rebuild with `--features metal` and run on Apple Silicon."
            )
        }),
        other => panic!("unknown --device {other:?} (use cpu | metal)"),
    }
}

fn main() {
    let args = parse_args();
    let embedder_dir = env_path("LUNARIS_EMBEDDER_DIR").unwrap_or_else(|| {
        panic!(
            "LUNARIS_EMBEDDER_DIR is required (dir with model.safetensors + \
             tokenizer.json + config.json — same layout the production \
             resolver in crates/lunaris/src/handle.rs expects)"
        )
    });
    let tokenizer_path = embedder_dir.join("tokenizer.json");
    let config_path = embedder_dir.join("config.json");
    let device = resolve_device(&args.device);

    let gguf_path = env_path("LUNARIS_EMBEDDER_GGUF");
    let use_gguf = match args.quant.as_deref() {
        Some("gguf") => true,
        Some("fp") => false,
        Some(other) => panic!("unknown --quant {other:?} (use fp | gguf)"),
        None => gguf_path.is_some(),
    };

    let layer = StageTimingLayer::new();
    let subscriber = tracing_subscriber::registry().with(layer.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let corpus = synth_corpus(args.corpus);
    let refs: Vec<&str> = corpus.iter().map(String::as_str).collect();

    let label: &'static str =
        if use_gguf { "granite-r2 Q4_K_M (candle quantized)" } else { "granite-r2 FP32 (candle)" };

    if use_gguf {
        #[cfg(feature = "embedder-gguf")]
        {
            let gguf_path = gguf_path.unwrap_or_else(|| {
                panic!("--quant gguf (or auto-detect) requires LUNARIS_EMBEDDER_GGUF")
            });
            let embedder = NativeQuantizedEmbedder::open(NativeQuantizedEmbedderOpts {
                gguf_path,
                tokenizer_path,
                config_path,
                device,
            })
            .expect("open quantized embedder");
            for chunk in refs.chunks(args.batch) {
                let _ = embedder.embed_blocking(chunk).expect("embed_blocking (gguf)");
            }
        }
        #[cfg(not(feature = "embedder-gguf"))]
        {
            panic!(
                "--quant gguf requires building with `--features embedder-gguf` \
                 (LUNARIS_EMBEDDER_GGUF was set but this binary doesn't support it)"
            );
        }
    } else {
        let weights_path = embedder_dir.join("model.safetensors");
        let embedder = NativeEmbedder::open(NativeEmbedderOpts {
            weights_path,
            tokenizer_path,
            config_path,
            device,
        })
        .expect("open FP32 embedder");
        for chunk in refs.chunks(args.batch) {
            let _ = embedder.embed_blocking(chunk).expect("embed_blocking (fp32)");
        }
    }

    print_report(
        &layer.samples(),
        &format!("{label} — device={} batch={} corpus={}", args.device, args.batch, args.corpus),
    );
}
