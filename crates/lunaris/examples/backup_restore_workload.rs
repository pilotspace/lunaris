//! Backup/restore drill workload driver (0.6.2 task 10).
//!
//! Companion to [`scripts/backup-restore-drill.sh`]. This binary is the
//! *Lunaris* half of the disaster-recovery drill: it writes a real,
//! deterministic Lunaris workload through the production ingest path into a
//! live Moon, and it re-derives a byte-comparable **content fingerprint** from
//! the recall path so the script can prove the restored instance serves the
//! same data — not merely that it answers `PING`.
//!
//! ## Why no inference
//!
//! Durability is a storage property, not a quality property. The drill runs
//! with [`NoopEmbedder`] (zero vectors) so it needs neither GGUF weights nor a
//! C++ toolchain, and so the retrieval leg is **BM25/keyword**, which is
//! deterministic. Vector recall over all-zero embeddings degenerates to HNSW
//! tie-break order, which legitimately permutes across an index rebuild
//! (`docs/durability.md` §2.2b) and would make the fingerprint comparison
//! meaningless.
//!
//! ## Modes
//!
//! ```bash
//! # write N deterministic episodes, emit the expected fingerprint
//! cargo run -p lunaris-memory --no-default-features \
//!   --example backup_restore_workload -- \
//!   write --url moon://127.0.0.1:6395 --scope drill-abc --docs 200 --out before.json
//!
//! # re-derive the fingerprint from a (possibly restored) instance
//! cargo run -p lunaris-memory --no-default-features \
//!   --example backup_restore_workload -- \
//!   verify --url moon://127.0.0.1:6396 --scope drill-abc --docs 200 --out after.json
//! ```
//!
//! `verify` exits 0 whether or not the data is present — judging equivalence
//! is the script's job (it diffs the two JSON documents). It exits non-zero
//! only when it cannot talk to the backend at all, which is itself a signal
//! the script interprets (a restored instance that refuses to load).
//!
//! ## Fingerprint shape
//!
//! * `corpus` — one BM25 query for a token present in **every** document,
//!   yielding the whole corpus in one shot: hit count + the sorted, verbatim
//!   chunk texts. This is the count-AND-content equivalence check.
//! * `per_doc` — one BM25 query per document for its unique marker token, so
//!   a partial restore names exactly which documents were lost rather than
//!   just reporting a smaller number.
//!
//! Both legs sort their outputs, so post-replay ranking permutation
//! (`docs/durability.md` §2.2b) cannot produce a false mismatch.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lunaris::{EpisodeBuilder, Lunaris};
use lunaris_core::{Embedder, NoopEmbedder, Scope};
use lunaris_retrieve::{Keyword, Query};
use serde_json::{Value, json};

/// Token embedded in every drill document — the corpus-wide BM25 probe.
const CORPUS_TOKEN: &str = "lunarisdrillcorpus";

/// Parsed CLI arguments. Hand-rolled so the example needs no `clap`.
struct Args {
    mode: Mode,
    url: String,
    scope: String,
    docs: usize,
    out: Option<String>,
    /// `verify` only: poll until the corpus probe returns this many hits.
    expect_hits: Option<usize>,
    /// `verify` only: how long to poll for `expect_hits` before giving up.
    settle_timeout_secs: f64,
}

#[derive(PartialEq, Eq)]
enum Mode {
    Write,
    Verify,
}

fn usage() -> String {
    "usage: backup_restore_workload <write|verify> --url <moon-url> --scope <scope> \
     --docs <n> [--out <path>] [--expect-hits <n>] [--settle-timeout-secs <f>]"
        .to_string()
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mode = match it.next().as_deref() {
        Some("write") => Mode::Write,
        Some("verify") => Mode::Verify,
        other => return Err(format!("unknown mode {other:?}\n{}", usage())),
    };
    let mut url = String::new();
    let mut scope = String::new();
    let mut docs = 0usize;
    let mut out = None;
    let mut expect_hits = None;
    let mut settle_timeout_secs = 30.0_f64;
    while let Some(flag) = it.next() {
        let val = it.next().ok_or_else(|| format!("{flag} needs a value\n{}", usage()))?;
        match flag.as_str() {
            "--url" => url = val,
            "--scope" => scope = val,
            "--docs" => docs = val.parse().map_err(|e| format!("--docs: {e}"))?,
            "--out" => out = Some(val),
            "--expect-hits" => {
                expect_hits = Some(val.parse().map_err(|e| format!("--expect-hits: {e}"))?)
            }
            "--settle-timeout-secs" => {
                settle_timeout_secs =
                    val.parse().map_err(|e| format!("--settle-timeout-secs: {e}"))?
            }
            other => return Err(format!("unknown flag {other}\n{}", usage())),
        }
    }
    if url.is_empty() || scope.is_empty() || docs == 0 {
        return Err(format!("--url, --scope and --docs are required\n{}", usage()));
    }
    Ok(Args { mode, url, scope, docs, out, expect_hits, settle_timeout_secs })
}

/// Deterministic `(source, marker, body)` for document `i`.
///
/// The body is short enough that the chunker emits exactly one chunk, and it
/// repeats the marker so BM25 scores it well above the corpus token alone.
fn doc(i: usize) -> (String, String, String) {
    let source = format!("drill:doc/{i:05}");
    let marker = format!("drillmarker{i:05}");
    let body = format!(
        "{CORPUS_TOKEN} {marker}. Lunaris backup and restore drill document number {i}. \
         This body is stable filler so the chunker output is byte-deterministic across \
         runs and across a restore onto a different host. End of record {marker}."
    );
    (source, marker, body)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(768));
    let handle = match Lunaris::open_with_embedder(&args.url, embedder).await {
        Ok(h) => h,
        Err(e) => {
            // Connect failure is a legitimate drill OUTCOME (a restored dir the
            // server refuses to load, a stale-version handshake rejection...).
            // Emit a machine-readable marker so the script can distinguish it
            // from an empty-but-healthy instance.
            eprintln!("CONNECT_FAILED {e}");
            if let Some(path) = &args.out {
                let _ = std::fs::write(
                    path,
                    serde_json::to_vec_pretty(&json!({
                        "scope": args.scope,
                        "docs": args.docs,
                        "connect_failed": e.to_string(),
                    }))
                    .unwrap_or_default(),
                );
            }
            return ExitCode::from(3);
        }
    };
    let scope = match Scope::new(args.scope.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bad scope: {e}");
            return ExitCode::from(2);
        }
    };
    let scoped = handle.scoped(scope);

    let mut report = json!({
        "scope": args.scope,
        "docs": args.docs,
        "url": args.url,
        "mode": if args.mode == Mode::Write { "write" } else { "verify" },
    });

    if args.mode == Mode::Write {
        let t0 = Instant::now();
        for i in 0..args.docs {
            let (source, _marker, body) = doc(i);
            if let Err(e) = scoped.ingest(EpisodeBuilder::new(source.clone(), body)).await {
                eprintln!("ingest {i} failed: {e}");
                return ExitCode::from(4);
            }
        }
        let ingest_secs = t0.elapsed().as_secs_f64();
        report["ingest_secs"] = json!(ingest_secs);
        eprintln!("ingested {} docs in {ingest_secs:.2}s", args.docs);
    }

    // ── Settle poll (verify): the FT index can lag the acked HSETs, and a
    // restored instance rebuilds it during startup. Poll rather than sleep so
    // the measured "time to first correct recall" is real, not padded.
    if let Some(expect) = args.expect_hits {
        let t0 = Instant::now();
        let settle_secs = loop {
            let n = corpus_probe(&scoped, args.docs).await.map(|v| v.len()).unwrap_or(0);
            if n >= expect {
                break t0.elapsed().as_secs_f64();
            }
            if t0.elapsed().as_secs_f64() > args.settle_timeout_secs {
                let elapsed = t0.elapsed().as_secs_f64();
                eprintln!("settle TIMEOUT after {elapsed:.2}s ({n}/{expect} hits)");
                break elapsed;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        report["settle_secs"] = json!(settle_secs);
    }

    // ── Corpus leg: one query, whole corpus, sorted verbatim texts.
    let corpus_texts = match corpus_probe(&scoped, args.docs).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("corpus probe failed: {e}");
            Vec::new()
        }
    };
    report["corpus"] = json!({
        "query": CORPUS_TOKEN,
        "hit_count": corpus_texts.len(),
        "texts": corpus_texts,
    });

    // ── Per-doc leg: name exactly which documents are missing.
    let mut missing: Vec<String> = Vec::new();
    let mut found = 0usize;
    for i in 0..args.docs {
        let (_source, marker, _body) = doc(i);
        let hits = scoped
            .dsl()
            .with_root(Keyword::bm25("chunks", 20).top(5))
            .execute(Query::text(&marker))
            .await
            .unwrap_or_default();
        if hits.iter().any(|h| h.text.contains(&marker)) {
            found += 1;
        } else {
            missing.push(marker);
        }
    }
    report["per_doc"] = json!({ "found": found, "missing": missing });

    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into()));
    if let Some(path) = &args.out
        && let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap_or_default())
    {
        eprintln!("could not write {path}: {e}");
        return ExitCode::from(5);
    }
    ExitCode::SUCCESS
}

/// BM25 the corpus-wide token and return every hit's chunk text, **sorted**.
///
/// Sorting is what makes the fingerprint immune to post-replay ranking
/// permutation (`docs/durability.md` §2.2b) while staying a true content
/// comparison: a lost or corrupted document changes the sorted list.
async fn corpus_probe(
    scoped: &lunaris::ScopedLunaris<'_>,
    docs: usize,
) -> Result<Vec<String>, lunaris_core::LunarisError> {
    // Over-fetch: k must exceed the corpus so a healthy instance is never
    // truncated into a false "loss" signal.
    let k = docs.saturating_mul(2).max(64);
    let hits = scoped
        .dsl()
        .with_root(Keyword::bm25("chunks", k).top(k))
        .execute(Query::text(CORPUS_TOKEN))
        .await?;
    let mut texts: Vec<String> =
        hits.into_iter().filter(|h| h.text.contains(CORPUS_TOKEN)).map(|h| h.text).collect();
    texts.sort();
    texts.dedup();
    Ok(texts)
}

/// Keeps `Value` in scope for the `json!` macro's inferred type in older
/// rustc diagnostics; also documents the report shape at the type level.
#[allow(dead_code)]
fn _report_type_marker(v: Value) -> Value {
    v
}
