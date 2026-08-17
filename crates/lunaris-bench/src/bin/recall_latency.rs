//! GA-2b — recall-latency envelope at the target corpus (100k docs).
//!
//! Measures steady-state latency of THE unified production recall root
//! (`lunaris_retrieve::production_root`, GA-1) on a live scratch Moon, at a
//! stated target corpus, for the three production configs:
//!
//! - `baseline` — graph OFF, rerank OFF (the shipped default)
//! - `rerank`   — the GA-1 opt-in cross-encoder stage (`LUNARIS_RECALL_RERANK`),
//!   real bge-reranker-v2-m3 Q5_K_M in the loop (needs `--features llamacpp[,metal]`)
//! - `graph`    — graph pipeline ON (chunks ∧ facts legs, Navigate + BM25)
//!
//! ## Methodology — retrieval-only (the contract decomposition)
//!
//! Query embedding runs through `StubEmbedder` (microseconds), exactly the
//! decomposition the v0.2.x strict-replay + v0.7 rerun methodology
//! established for the sub-25 ms contract: embed out of the loop,
//! engine (FT.SEARCH + RRF fuse + hydrate [+ rerank]) in the loop. Corpus
//! chunks are ingested through the REAL public ingest paths
//! (`ScopedLunaris::ingest` / `ingest_structured`) with the same
//! `StubEmbedder`, so stored vectors and query vectors are consistent and
//! the vector index does real 768-d KNN work. The rerank config is the one
//! config with real model inference in the timed path — that stage's cost
//! is precisely what GA-2b prices.
//!
//! ## Resumable ingest
//!
//! `ingest --start N --end M` writes docs `[N, M)` so the shell runner can
//! chunk the corpus build into bounded foreground calls. Doc content is
//! deterministic in the doc index (seeded splitmix64), so any chunking
//! yields the same corpus.
//!
//! ## Port guard
//!
//! Refuses moon URLs on ports 6379/6380/6381 (live dev/personal stores —
//! see `scripts/bench/lme/lib.sh` GUARD 1 and auto-memory
//! `feedback_bench_moon_never_6381`). This binary is meant for a throwaway
//! bench Moon on 6399+.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use lunaris::{
    EntityInput, EpisodeBuilder, FactInput, Lunaris, Query, RecallRerankConfig, Scope,
    StructuredIngest,
};
use lunaris_core::StubEmbedder;
use serde::Serialize;

/// Deterministic corpus seed — part of the reproduction contract.
const CORPUS_SEED: u64 = 0x6A2B_2026_0818_0001;

/// Embedding dimension — matches the production granite embedder (768-d)
/// so the vector index does the same-shaped KNN work.
const EMBED_DIM: usize = 768;

/// The bench scope. Alphabet-legal per RFC 0001.
const BENCH_SCOPE: &str = "gabench";

/// Ports this binary refuses to touch: 6379 (system Redis), 6380 (dev
/// Moon), 6381 (operator's live personal memory store).
const RESERVED_PORTS: &[u16] = &[6379, 6380, 6381];

#[derive(Parser, Debug)]
#[command(name = "recall-latency", about = "GA-2b recall latency envelope harness")]
struct Cli {
    /// Target Moon URL, e.g. moon://127.0.0.1:6399. Reserved ports refused.
    #[arg(long, env = "GA2B_MOON_URL")]
    moon_url: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Ingest docs [start, end) of the deterministic corpus.
    Ingest {
        #[arg(long)]
        start: usize,
        #[arg(long)]
        end: usize,
        /// Every Nth doc goes through ingest_structured with 1 entity +
        /// 1 fact (populates the graph-ON fact/entity legs). 0 = never.
        #[arg(long, default_value_t = 5)]
        facts_every: usize,
        /// In-flight concurrent ingests.
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
    },
    /// Run the timed recall pass for one config.
    Measure {
        /// baseline | rerank | graph
        #[arg(long)]
        config: String,
        #[arg(long, default_value_t = 500)]
        queries: usize,
        #[arg(long, default_value_t = 50)]
        warmup: usize,
        /// Echoed into the results JSON (corpus size the operator ingested).
        #[arg(long, default_value_t = 0)]
        docs_hint: usize,
        /// Write the results JSON here (also printed to stdout).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Also dump the raw per-query samples (one ms value per line) —
        /// lets long configs (rerank is ~1 s/query) be split across
        /// bounded runs and merged offline.
        #[arg(long)]
        dump_samples: Option<PathBuf>,
        /// Offset into the deterministic query sequence (so split runs
        /// use disjoint queries).
        #[arg(long, default_value_t = 0)]
        query_offset: usize,
    },
}

// ---------------------------------------------------------------------------
// Deterministic corpus + query generation
// ---------------------------------------------------------------------------

/// Topic vocabulary the docs and queries share. Realistic agent-memory
/// prose fragments — a few hundred bytes per doc.
const VOCAB: &[&str] = &[
    "deployment",
    "rollback",
    "latency",
    "checkpoint",
    "retriever",
    "episode",
    "consolidation",
    "gradient",
    "pipeline",
    "schema",
    "migration",
    "replica",
    "quorum",
    "snapshot",
    "compaction",
    "index",
    "shard",
    "partition",
    "timeout",
    "retry",
    "budget",
    "alert",
    "dashboard",
    "tracing",
    "handler",
    "session",
    "context",
    "recall",
    "ingest",
    "fusion",
    "ranking",
    "embedding",
    "temporal",
    "provenance",
    "verifier",
    "extractor",
    "entity",
    "relation",
    "confidence",
    "review",
    "incident",
    "postmortem",
    "capacity",
    "throughput",
    "saturation",
    "backlog",
    "worker",
    "queue",
    "commit",
    "branch",
    "release",
    "canary",
    "rollout",
    "traffic",
];

const SUBJECTS: &[&str] = &[
    "the ops agent",
    "the planning agent",
    "the review bot",
    "the ingest worker",
    "the retrieval layer",
    "the memory engine",
    "the deploy pipeline",
    "the oncall engineer",
];

const VERBS: &[&str] = &[
    "observed",
    "recorded",
    "flagged",
    "resolved",
    "escalated",
    "measured",
    "compared",
    "archived",
    "promoted",
    "reverted",
];

/// Cheap deterministic PRNG per index — splitmix64. Enough entropy for
/// vocabulary sampling; no rand dependency needed in the hot loop.
fn mix(seed: u64, i: u64, salt: u64) -> u64 {
    let mut z = seed ^ (i.wrapping_mul(0x9E3779B97F4A7C15)) ^ salt.wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn pick<'a>(pool: &[&'a str], seed: u64, i: u64, salt: u64) -> &'a str {
    pool[(mix(seed, i, salt) % pool.len() as u64) as usize]
}

/// Deterministic ~300-byte doc for index `i`. Carries a selective topic
/// marker (`topic-<i mod 1000>`) so BM25 has discriminative terms, plus
/// shared vocabulary so queries land non-empty result sets.
fn doc_text(i: usize) -> String {
    let i64v = i as u64;
    let topic = i % 1000;
    let mut body = format!(
        "note {i} on topic-{topic}: {} {} a {} regression in the {} path. ",
        pick(SUBJECTS, CORPUS_SEED, i64v, 1),
        pick(VERBS, CORPUS_SEED, i64v, 2),
        pick(VOCAB, CORPUS_SEED, i64v, 3),
        pick(VOCAB, CORPUS_SEED, i64v, 4),
    );
    for s in 0..4u64 {
        body.push_str(&format!(
            "The {} {} correlated with {} {} under {} pressure. ",
            pick(VOCAB, CORPUS_SEED, i64v, 10 + s),
            pick(VERBS, CORPUS_SEED, i64v, 20 + s),
            pick(VOCAB, CORPUS_SEED, i64v, 30 + s),
            pick(VOCAB, CORPUS_SEED, i64v, 40 + s),
            pick(VOCAB, CORPUS_SEED, i64v, 50 + s),
        ));
    }
    body
}

/// Deterministic query for query index `j` — same vocabulary distribution
/// as the docs, with a topic marker so hits are non-empty.
fn query_text(j: usize) -> String {
    let j64 = j as u64;
    format!(
        "what happened with the {} {} on topic-{} under {} pressure",
        pick(VOCAB, CORPUS_SEED, j64, 103),
        pick(VOCAB, CORPUS_SEED, j64, 104),
        j % 1000,
        pick(VOCAB, CORPUS_SEED, j64, 105),
    )
}

// ---------------------------------------------------------------------------
// Port guard
// ---------------------------------------------------------------------------

/// Extract the port from a `moon://host:port` URL and refuse reserved ones.
fn guard_moon_url(url: &str) -> Result<u16> {
    let parsed = url::Url::parse(url).with_context(|| format!("unparseable moon url: {url}"))?;
    let port = parsed.port().context("moon url must carry an explicit port")?;
    if RESERVED_PORTS.contains(&port) {
        bail!(
            "moon url port {port} is RESERVED (live dev/personal store). \
             Stand up a throwaway bench Moon on 6399+ instead."
        );
    }
    Ok(port)
}

// ---------------------------------------------------------------------------
// Results envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct MeasureResults {
    config: String,
    corpus_docs_hint: usize,
    k: usize,
    queries: usize,
    warmup: usize,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    /// Wall time of the very first recall on the fresh handle — for the
    /// rerank config this includes the one-time lazy GGUF model load.
    first_call_ms: f64,
    avg_hits: f64,
    zero_hit_queries: usize,
    embedder: String,
    methodology: String,
    workspace_sha: String,
    run_timestamp_iso: String,
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (((sorted.len() as f64) * pct / 100.0) as usize).min(sorted.len() - 1);
    sorted[idx]
}

fn workspace_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    guard_moon_url(&cli.moon_url)?;

    match cli.cmd {
        Cmd::Ingest { start, end, facts_every, concurrency } => {
            run_ingest(&cli.moon_url, start, end, facts_every, concurrency).await
        }
        Cmd::Measure { config, queries, warmup, docs_hint, out, dump_samples, query_offset } => {
            run_measure(
                &cli.moon_url,
                &config,
                queries,
                warmup,
                docs_hint,
                out,
                dump_samples,
                query_offset,
            )
            .await
        }
    }
}

async fn open_handle(url: &str) -> Result<Lunaris> {
    let handle = Lunaris::open(url).await.context("Lunaris::open")?;
    // Retrieval-only methodology: deterministic stub embedder for both
    // corpus and query vectors (see module docs). The handle's lazy
    // production reranker (llamacpp feature) is left in place — the rerank
    // config is the one place real inference belongs in the timed path.
    Ok(handle.with_embedder(Arc::new(StubEmbedder::new(EMBED_DIM))))
}

async fn run_ingest(
    url: &str,
    start: usize,
    end: usize,
    facts_every: usize,
    concurrency: usize,
) -> Result<()> {
    if end <= start {
        bail!("--end must be greater than --start");
    }
    let handle = open_handle(url).await?;
    let scoped = handle.scoped(Scope::new(BENCH_SCOPE)?);
    let t0 = Instant::now();
    let total = end - start;
    let done = std::sync::atomic::AtomicUsize::new(0);

    futures::stream::iter(start..end)
        .map(|i| {
            let scoped = &scoped;
            let done = &done;
            async move {
                let text = doc_text(i);
                let source = format!("gabench:doc/{i}");
                let res = ingest_one_with_retry(scoped, i, facts_every, &source, &text).await;
                if let Err(e) = res {
                    eprintln!("ingest doc {i} failed: {e}");
                    return Err(anyhow::anyhow!("ingest doc {i}: {e}"));
                }
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n.is_multiple_of(5000) {
                    let dt = t0.elapsed().as_secs_f64();
                    eprintln!(
                        "[ingest] {n}/{total} ({:.0} docs/s, {:.0}s elapsed)",
                        n as f64 / dt,
                        dt
                    );
                }
                Ok(())
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect::<Vec<Result<()>>>()
        .await
        .into_iter()
        .collect::<Result<Vec<()>>>()?;

    let dt = t0.elapsed().as_secs_f64();
    eprintln!("[ingest] DONE range [{start}, {end}) in {dt:.1}s ({:.0} docs/s)", total as f64 / dt);
    Ok(())
}

/// One ingest, retried on transient backend pressure (`busy: compaction
/// backlog`) with linear backoff. Design-for-failure: a 100k-doc corpus
/// build WILL outrun Moon's flush pipeline occasionally; a transient busy
/// must stall the producer, not abort the build. Non-busy errors fail fast.
async fn ingest_one_with_retry(
    scoped: &lunaris::ScopedLunaris<'_>,
    i: usize,
    facts_every: usize,
    source: &str,
    text: &str,
) -> Result<(), lunaris::LunarisError> {
    const MAX_ATTEMPTS: usize = 8;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let res = if facts_every > 0 && i.is_multiple_of(facts_every) {
            let now = Utc::now();
            let subject = format!("Service-{}", i % 500);
            let object = pick(VOCAB, CORPUS_SEED, i as u64, 77).to_string();
            let payload =
                StructuredIngest::new(EpisodeBuilder::new(source.to_string(), text.to_string()))
                    .with_entities(vec![EntityInput {
                        name: subject.clone(),
                        entity_type: "Service".into(),
                        aliases: vec![],
                        confidence: 1.0,
                        valid_from: now,
                        valid_to: None,
                        embedding: None,
                    }])
                    .with_facts(vec![FactInput {
                        fact_text: format!(
                            "{subject} {} a {} issue on topic-{}",
                            pick(VERBS, CORPUS_SEED, i as u64, 78),
                            object,
                            i % 1000
                        ),
                        subject_name: subject,
                        subject_type: "Service".into(),
                        predicate: "observed".into(),
                        object_name: object,
                        object_type: "Concept".into(),
                        confidence: 1.0,
                        valid_from: now,
                        valid_to: None,
                    }]);
            scoped.ingest_structured(payload).await.map(|_| ())
        } else {
            scoped
                .ingest(EpisodeBuilder::new(source.to_string(), text.to_string()))
                .await
                .map(|_| ())
        };
        match res {
            Ok(()) => return Ok(()),
            // Transient backend pressure: compaction backlog ("busy") or an
            // AOF-rewrite-window fsync refusal ("AOF fsync failed; write not
            // durable" — observed once at ~93k docs during the 100k build;
            // the write is rolled back and succeeds on retry).
            Err(e)
                if attempt < MAX_ATTEMPTS
                    && (e.to_string().contains("busy") || e.to_string().contains("fsync")) =>
            {
                let backoff = std::time::Duration::from_millis(250 * attempt as u64);
                eprintln!(
                    "[ingest] doc {i} transient busy (attempt {attempt}) — backing off {backoff:?}"
                );
                tokio::time::sleep(backoff).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_measure(
    url: &str,
    config: &str,
    queries: usize,
    warmup: usize,
    docs_hint: usize,
    out: Option<PathBuf>,
    dump_samples: Option<PathBuf>,
    query_offset: usize,
) -> Result<()> {
    let mut handle = open_handle(url).await?;
    match config {
        "baseline" => {}
        "rerank" => {
            // Honor the production depth knob so the runner can measure the
            // stage at different pool sizes (unset → the 2*k = 60 default).
            let top_in = std::env::var(lunaris::RECALL_RERANK_TOP_IN_ENV_VAR)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&n| n > 0);
            handle = handle.with_recall_rerank(RecallRerankConfig { enabled: true, top_in });
        }
        "graph" => {
            handle.graph_pipeline().enable();
        }
        other => bail!("unknown --config '{other}' (expected baseline|rerank|graph)"),
    }
    let scoped = handle.scoped(Scope::new(BENCH_SCOPE)?);

    // First call on the fresh handle — separately reported (for the rerank
    // config this is where the lazy bge GGUF loads).
    let t_first = Instant::now();
    let first_hits = scoped.recall(Query::text(query_text(query_offset))).await?;
    let first_call_ms = t_first.elapsed().as_secs_f64() * 1000.0;
    eprintln!("[measure:{config}] first call {first_call_ms:.1} ms ({} hits)", first_hits.len());
    if first_hits.is_empty() {
        bail!("sanity recall returned 0 hits — corpus missing? Run `ingest` first.");
    }

    // Warmup (untimed).
    for j in 1..=warmup {
        let _ = scoped.recall(Query::text(query_text(query_offset + j))).await?;
    }

    // Timed steady-state pass — sequential, one in-flight query (matches
    // the strict-replay precedent; concurrency scaling is a separate study).
    let mut samples_ms: Vec<f64> = Vec::with_capacity(queries);
    let mut hits_total = 0usize;
    let mut zero_hits = 0usize;
    for j in 0..queries {
        let q = query_text(query_offset + warmup + 1 + j);
        let t0 = Instant::now();
        let hits = scoped.recall(Query::text(&q)).await?;
        samples_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        hits_total += hits.len();
        if hits.is_empty() {
            zero_hits += 1;
        }
        if (j + 1) % 100 == 0 {
            eprintln!("[measure:{config}] {}/{queries}", j + 1);
        }
    }

    let mut sorted = samples_ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let results = MeasureResults {
        config: config.to_string(),
        corpus_docs_hint: docs_hint,
        k: 30,
        queries,
        warmup,
        mean_ms: samples_ms.iter().sum::<f64>() / samples_ms.len().max(1) as f64,
        p50_ms: percentile(&sorted, 50.0),
        p95_ms: percentile(&sorted, 95.0),
        p99_ms: percentile(&sorted, 99.0),
        max_ms: sorted.last().copied().unwrap_or(0.0),
        first_call_ms,
        avg_hits: hits_total as f64 / queries.max(1) as f64,
        zero_hit_queries: zero_hits,
        embedder: format!("StubEmbedder({EMBED_DIM}) — retrieval-only decomposition"),
        methodology: "retrieval-only (embed out of loop; v0.2.x strict-replay decomposition)"
            .to_string(),
        workspace_sha: workspace_sha(),
        run_timestamp_iso: Utc::now().to_rfc3339(),
    };

    if let Some(path) = dump_samples {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lines: String = samples_ms.iter().map(|s| format!("{s}\n")).collect();
        std::fs::write(&path, lines)?;
        eprintln!("[measure:{config}] raw samples -> {}", path.display());
    }

    let json = serde_json::to_string_pretty(&results)?;
    println!("{json}");
    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &json)?;
        eprintln!("[measure:{config}] wrote {}", path.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_refuses_reserved_ports() {
        for p in [6379u16, 6380, 6381] {
            assert!(guard_moon_url(&format!("moon://127.0.0.1:{p}")).is_err(), "port {p}");
        }
        assert_eq!(guard_moon_url("moon://127.0.0.1:6399").unwrap(), 6399);
    }

    #[test]
    fn guard_requires_explicit_port() {
        assert!(guard_moon_url("moon://127.0.0.1").is_err());
    }

    #[test]
    fn corpus_is_deterministic_and_sized() {
        let a = doc_text(12345);
        let b = doc_text(12345);
        assert_eq!(a, b);
        assert_ne!(doc_text(1), doc_text(2));
        // "a few hundred bytes each" — the stated generation contract.
        let lens: Vec<usize> = (0..200).map(|i| doc_text(i * 500).len()).collect();
        assert!(lens.iter().all(|&l| (200..600).contains(&l)), "doc sizes {lens:?}");
    }

    #[test]
    fn queries_are_deterministic_and_share_vocab() {
        assert_eq!(query_text(7), query_text(7));
        assert!(query_text(7).contains("topic-7"));
    }

    #[test]
    fn percentile_basic() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert_eq!(percentile(&v, 50.0), 6.0);
        assert_eq!(percentile(&v, 99.0), 10.0);
        assert_eq!(percentile(&[], 50.0), 0.0);
    }
}
