//! `profile_rerank` — corpus-replay profiler for the bge-reranker-v2-m3
//! cross-encoder hot path. Attributes p50/p90/p99 latency to the
//! `lunaris.rerank.{tokenize_pairs,forward,score_extract,copy}` stages
//! instrumented in `reranker.rs`/`xlmr_reranker.rs`/`quantized_reranker.rs`/
//! `quantized_xlmr.rs`, plus tokens/s (forward-stage throughput) and the
//! padding-waste ratio (`padded_tokens / real_tokens`, recorded on the
//! `tokenize_pairs` span).
//!
//! This is Workstream C of
//! `docs/design/quantized-inference-extractor-reranker.md` §4b: the
//! reranker sits on every production recall, so its per-stage attribution
//! is the highest-value half of the microscope. Decision rule (§4b): if
//! ≥60% of p50 lives inside `forward` → the candle-vs-llama.cpp runtime
//! swap (§4c) is the lever; if it's tokenize/batch/copy → fix in place.
//!
//! ## Usage
//!
//! ```bash
//! # FP32 path (needs LUNARIS_RERANKER_DIR/model.safetensors on disk):
//! LUNARIS_RERANKER_DIR=~/.cache/lunaris/models/bge-reranker-v2-m3 \
//!   cargo run -p lunaris-rerank-native --example profile_rerank -- \
//!     --device cpu --batch 8 --corpus 8 --k 10
//!
//! # Q5_K_M-imatrix GGUF path (needs the `reranker-gguf` feature + a staged GGUF):
//! LUNARIS_RERANKER_DIR=~/.cache/lunaris/models/bge-reranker-v2-m3 \
//! LUNARIS_RERANKER_GGUF=~/.lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf \
//!   cargo run -p lunaris-rerank-native --features reranker-gguf --example profile_rerank -- \
//!     --device cpu --batch 8 --corpus 8 --k 10 --quant gguf
//!
//! # Metal (release build recommended; requires the `metal` feature):
//!   cargo run --release -p lunaris-rerank-native --features metal \
//!     --example profile_rerank -- --device metal --batch 8 --corpus 8 --k 10
//! ```
//!
//! ## Env vars (mirror the production resolver — see
//! `crates/lunaris/src/handle.rs`'s `resolve_reranker`)
//!
//! - `LUNARIS_RERANKER_DIR` — directory containing `model.safetensors`
//!   (FP32 path), `tokenizer.json`, `config.json`. REQUIRED (tokenizer +
//!   config always come from here, even in `gguf` quant mode).
//! - `LUNARIS_RERANKER_GGUF` — path to the Q5_K_M-imatrix GGUF; only
//!   consulted when `--quant gguf` is passed (or auto-selected because the
//!   env var is set and `--quant` was omitted).
//!
//! ## Flags
//!
//! - `--device cpu|metal` (default `cpu`)
//! - `--batch N` (default 8) — pair-batch size per forward pass
//! - `--corpus N` (default 8) — number of distinct rerank calls (queries) to replay
//! - `--k N` (default 10) — candidate docs per query (the K in "rerank K docs")
//! - `--quant fp|gguf` (default: auto — `gguf` iff `LUNARIS_RERANKER_GGUF`
//!   is set AND the crate was built with `--features reranker-gguf`)

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use candle_core::Device;
use lunaris_rerank_native::{NativeReranker, NativeRerankerOpts};
#[cfg(feature = "reranker-gguf")]
use lunaris_rerank_native::{NativeQuantizedReranker, NativeQuantizedRerankerOpts};
use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

// ── deterministic variable-length synthetic corpus ─────────────────────────

/// Same fixed vocabulary as `lunaris-embed-native/examples/profile_embed.rs`
/// (kept in sync deliberately — both profilers should read comparably in a
/// side-by-side terminal).
const WORD_BANK: &[&str] = &[
    "the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "async", "runtime",
    "candle", "tensor", "forward", "pass", "quantized", "matmul", "kernel", "padding", "batch",
    "token", "embedding", "granite", "reranker", "cross", "encoder", "attention", "layer",
    "normalize", "cosine", "similarity", "graph", "memory", "agent", "recall", "scope", "moon",
    "storage", "vector", "index", "search", "query", "document", "chunk", "summary", "entity",
    "relation", "fact", "community", "hybrid", "fusion",
];

/// SplitMix64 — see `profile_embed.rs`'s copy of this function for the full
/// rationale (deterministic, dependency-free PRNG for reproducible corpora).
fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Build `k` synthetic candidate documents for rerank call `call_idx`, with
/// REALISTIC length variance (mix of short chunks through RAPTOR-style long
/// summaries) — the same bucket distribution `profile_embed.rs` uses, offset
/// by `call_idx` so successive rerank calls see different (but deterministic)
/// corpora.
fn synth_docs(call_idx: usize, k: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(k);
    for j in 0..k {
        let i = call_idx * 1_000 + j; // unique seed per (call, doc) pair
        let (lo, hi) = match j % 10 {
            0..=3 => (16, 32),
            4..=6 => (64, 128),
            7..=8 => (200, 350),
            _ => (400, 512),
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

const QUERY: &str = "what is the tokens-per-second throughput of the candle quantized matmul kernel";

// ── per-stage timing layer (mirrors profile_embed.rs) ──────────────────────

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
    k: usize,
    quant: Option<String>,
}

fn parse_args() -> Args {
    let mut device = "cpu".to_string();
    let mut batch = 8usize;
    let mut corpus = 8usize;
    let mut k = 10usize;
    let mut quant = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--device" => device = it.next().unwrap_or_else(|| "cpu".into()),
            "--batch" => batch = it.next().and_then(|v| v.parse().ok()).unwrap_or(8),
            "--corpus" => corpus = it.next().and_then(|v| v.parse().ok()).unwrap_or(8),
            "--k" => k = it.next().and_then(|v| v.parse().ok()).unwrap_or(10),
            "--quant" => quant = it.next(),
            other => eprintln!("[profile_rerank] ignoring unknown arg: {other}"),
        }
    }
    Args { device, batch, corpus, k, quant }
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
    let reranker_dir = env_path("LUNARIS_RERANKER_DIR").unwrap_or_else(|| {
        panic!(
            "LUNARIS_RERANKER_DIR is required (dir with model.safetensors + \
             tokenizer.json + config.json — same layout the production \
             resolver in crates/lunaris/src/handle.rs expects)"
        )
    });
    let tokenizer_path = reranker_dir.join("tokenizer.json");
    let config_path = reranker_dir.join("config.json");
    let device = resolve_device(&args.device);

    let gguf_path = env_path("LUNARIS_RERANKER_GGUF");
    let use_gguf = match args.quant.as_deref() {
        Some("gguf") => true,
        Some("fp") => false,
        Some(other) => panic!("unknown --quant {other:?} (use fp | gguf)"),
        None => gguf_path.is_some(),
    };

    let layer = StageTimingLayer::new();
    let subscriber = tracing_subscriber::registry().with(layer.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let label: &'static str = if use_gguf {
        "bge-reranker-v2-m3 Q5_K_M-imatrix (candle quantized)"
    } else {
        "bge-reranker-v2-m3 FP32 (candle)"
    };

    if use_gguf {
        #[cfg(feature = "reranker-gguf")]
        {
            let gguf_path = gguf_path.unwrap_or_else(|| {
                panic!("--quant gguf (or auto-detect) requires LUNARIS_RERANKER_GGUF")
            });
            let reranker = NativeQuantizedReranker::open(NativeQuantizedRerankerOpts {
                gguf_path,
                tokenizer_path,
                config_path,
                device,
            })
            .expect("open quantized reranker");
            for call_idx in 0..args.corpus {
                let docs = synth_docs(call_idx, args.k);
                for chunk in docs.chunks(args.batch) {
                    let pairs: Vec<(&str, &str)> =
                        chunk.iter().map(|d| (QUERY, d.as_str())).collect();
                    let _ = reranker.score_blocking(&pairs).expect("score_blocking (gguf)");
                }
            }
        }
        #[cfg(not(feature = "reranker-gguf"))]
        {
            panic!(
                "--quant gguf requires building with `--features reranker-gguf` \
                 (LUNARIS_RERANKER_GGUF was set but this binary doesn't support it)"
            );
        }
    } else {
        let weights_path = reranker_dir.join("model.safetensors");
        let reranker = NativeReranker::open(NativeRerankerOpts {
            weights_path,
            tokenizer_path,
            config_path,
            device,
        })
        .expect("open FP32 reranker");
        for call_idx in 0..args.corpus {
            let docs = synth_docs(call_idx, args.k);
            for chunk in docs.chunks(args.batch) {
                let pairs: Vec<(&str, &str)> = chunk.iter().map(|d| (QUERY, d.as_str())).collect();
                let _ = reranker.score_blocking(&pairs).expect("score_blocking (fp32)");
            }
        }
    }

    print_report(
        &layer.samples(),
        &format!(
            "{label} — device={} batch={} corpus={} k={}",
            args.device, args.batch, args.corpus, args.k
        ),
    );
}
