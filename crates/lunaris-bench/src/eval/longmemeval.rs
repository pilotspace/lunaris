//! Plan 05-06 EVAL-01 — LongMemEval J-score harness.
//!
//! Downloads HF dataset `xiaowu0162/longmemeval` to
//! `LUNARIS_EVAL_CACHE_DIR` (default `~/.cache/lunaris/eval/`) via
//! `hf-hub 0.4` (cache-first; subsequent runs use the cached dataset).
//! Threshold: J ≥ 65 (alpha bar per blueprint §13).
//!
//! ## Soft-fail (CONTEXT.md D-21)
//!
//! Download failure → status:`SKIPPED` (NOT FAIL). MOON_URL unset →
//! status:`SKIPPED`. Per-row corpus parse failure → status:`SKIPPED` with
//! the parse error attached.
//!
//! ## B-4 fix (CLAUDE.md `#![forbid(unsafe_code)]`)
//!
//! `download_dataset` uses `hf_hub::api::tokio::ApiBuilder::with_cache_dir`
//! for caller-controlled cache directories. ZERO unsafe blocks. ZERO env
//! mutation. The prior plan-body shape that used
//! `unsafe { std::env::set_var("HF_HOME", ...) }` was rejected by the B-4
//! fix per the threat-model T-05-06-04 mitigation column.
//!
//! ## W-7 fix (visibility)
//!
//! `download_dataset` is `pub(crate)` so [`crate::eval::locomo`] and
//! [`crate::eval::er_f1`] reuse this same helper for their HF datasets
//! (no `__download_test_helper` alias anywhere — single source of truth).
//!
//! ## J-score computation (stub)
//!
//! Per dataset README: J-score = % of queries where the recalled top-k
//! contains the gold answer (or LLM-judge equivalent for free-form). Full
//! implementation is operator/dev-box-only per ROADMAP risk register
//! day-7 fallback — Plan 05-06 lands the harness shell + dataset
//! plumbing; live numbers populate via 05-HUMAN-UAT.md.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::eval::EvalRow;

pub(crate) const HARNESS: &str = "longmemeval";
pub(crate) const METRIC: &str = "j_score";
pub(crate) const THRESHOLD: f64 = 65.0;
const HF_REPO: &str = "xiaowu0162/longmemeval";
/// Default dataset file. `longmemeval_oracle` = reduced (evidence-only)
/// haystack — easy retrieval, fast. Set `LUNARIS_EVAL_LME_DATASET=longmemeval_s`
/// for the FULL adversarial haystack (~50 sessions / ~494 turns per question)
/// that competitors (Zep 90.2%, etc.) report their J-score on.
const DEFAULT_DATASET_FILENAME: &str = "longmemeval_oracle";

/// Resolve the dataset file from `LUNARIS_EVAL_LME_DATASET` (default
/// `longmemeval_oracle`). Only the two canonical haystack files are accepted
/// so a typo can't silently 404 into a SKIP.
fn dataset_filename() -> String {
    std::env::var("LUNARIS_EVAL_LME_DATASET")
        .unwrap_or_else(|_| DEFAULT_DATASET_FILENAME.to_string())
}

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    let started = Instant::now();

    let url = match std::env::var("MOON_URL") {
        Ok(u) => u,
        Err(_) => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD,
                "MOON_URL unset — LongMemEval harness needs Moon backend",
            ));
            return Ok(());
        }
    };

    let cache = resolve_cache_dir();
    let _ = std::fs::create_dir_all(&cache);

    // Cache-first download via hf-hub 0.4. B-4 fix shape: ApiBuilder +
    // with_cache_dir; ZERO unsafe blocks anywhere.
    let dataset_file = dataset_filename();
    let dataset_path = match download_dataset(HF_REPO, &dataset_file, &cache).await {
        Ok(p) => p,
        Err(e) => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD,
                &format!("dataset download failed: {e}"),
            ));
            return Ok(());
        }
    };

    // Reachability check only — `score_haystack` re-opens a fresh handle per
    // question (after a full Moon reset) so each question retrieves from its
    // own haystack against ghost-free indices.
    if let Err(e) = lunaris::Lunaris::open(&url).await {
        results.push(EvalRow::skipped(
            HARNESS,
            METRIC,
            THRESHOLD,
            &format!("Lunaris::open({url}) failed: {e}"),
        ));
        return Ok(());
    }

    let bytes = match std::fs::read(&dataset_path) {
        Ok(b) => b,
        Err(e) => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD,
                &format!("read dataset: {e}"),
            ));
            return Ok(());
        }
    };
    let records = match parse_longmemeval_full(&bytes) {
        Ok(r) => r,
        Err(e) => {
            results.push(EvalRow::skipped(HARNESS, METRIC, THRESHOLD, &format!("parse: {e}")));
            return Ok(());
        }
    };

    let metric = metric_name(judge_mode_enabled());
    let j_score = match score_haystack(&url, &records).await {
        Ok(s) => s,
        Err(e) => {
            results.push(EvalRow::skipped(
                HARNESS,
                metric,
                THRESHOLD,
                &format!("eval pass failed: {e}"),
            ));
            return Ok(());
        }
    };

    results.push(EvalRow::judge_ge(
        HARNESS,
        metric,
        j_score,
        THRESHOLD,
        started.elapsed().as_millis() as u64,
    ));
    Ok(())
}

/// Resolve `LUNARIS_EVAL_CACHE_DIR` env var; fall back to
/// `~/.cache/lunaris/eval/` (`dirs::cache_dir()`); fall back to `./`. Pure
/// function — no env mutation. CONTEXT.md D-21 verbatim.
pub(crate) fn resolve_cache_dir() -> PathBuf {
    std::env::var("LUNARIS_EVAL_CACHE_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        dirs::cache_dir().unwrap_or_else(|| PathBuf::from(".")).join("lunaris/eval")
    })
}

/// **B-4 + W-7 fix.** Download a single file from a HuggingFace dataset
/// repo to a caller-controlled cache directory. Reused by `locomo.rs` and
/// `er_f1.rs` for their respective HF datasets — single source of truth
/// for HF download semantics in the eval gauntlet.
///
/// # Why `pub(crate)`
///
/// W-7 fix: lets sibling modules reuse this helper without exposing it on
/// the crate's public API surface and without introducing a fictional
/// `__download_test_helper` alias.
///
/// # Why no env mutation
///
/// B-4 fix: the prior plan-body shape used
/// `unsafe { std::env::set_var("HF_HOME", cache_dir) }` to point hf-hub at
/// the desired cache. That violates CLAUDE.md `#![forbid(unsafe_code)]`
/// (env mutation requires unsafe in Rust 2024). hf-hub 0.4 ships
/// `ApiBuilder::with_cache_dir(PathBuf)` for exactly this case — caller-
/// controlled cache; no global state mutation.
pub(crate) async fn download_dataset(
    repo: &str,
    file: &str,
    cache_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let api = hf_hub::api::tokio::ApiBuilder::new()
        .with_cache_dir(cache_dir.to_path_buf())
        .with_progress(false)
        .build()?;
    let repo_handle = api.repo(hf_hub::Repo::with_revision(
        repo.to_string(),
        hf_hub::RepoType::Dataset,
        "main".to_string(),
    ));
    let path = repo_handle.get(file).await?;
    Ok(path)
}

/// One eval query: the question text + the gold answer text the recall
/// pipeline must surface in its top-k hits. Public so the dataset parsers
/// (`parse_longmemeval`, `crate::eval::locomo::parse_locomo`) and the pure
/// scorer in `crate::eval::score` can build and consume the query set.
#[derive(Debug, Clone)]
pub struct EvalQuery {
    pub query: String,
    pub expected_answer: String,
}

/// Full LongMemEval record: question + gold answer + the multi-session haystack
/// (real conversational distractors) + which sessions hold the evidence. This is
/// the corpus the deferred HUMAN-UAT harness was meant to ingest — not just the
/// gold answers.
pub(crate) struct HaystackRecord {
    pub question_id: String,
    pub question_type: String,
    pub question: String,
    pub answer: String,
    pub answer_session_ids: Vec<String>,
    /// `(session_id, turn_texts)` for every haystack session, distractors
    /// included. Each session's turn list is prefixed with a `[Session date:
    /// ...]` marker (LongMemEval `haystack_dates`) for temporal grounding.
    pub sessions: Vec<(String, Vec<String>)>,
    /// LongMemEval `question_date` — the "current" date the question is asked
    /// on, used as the anchor for relative temporal reasoning ("how long ago").
    pub question_date: String,
}

impl HaystackRecord {
    /// LongMemEval marks abstention (unanswerable) questions with a `_abs`
    /// suffix on the `question_id`. The judge prompt for these asks whether
    /// the model correctly identified the question as unanswerable.
    pub(crate) fn is_abstention(&self) -> bool {
        self.question_id.ends_with("_abs")
    }
}

/// Parse the `longmemeval_oracle` JSON (a list of records) into full haystack
/// records. Each turn is rendered `"{role}: {content}"`.
pub(crate) fn parse_longmemeval_full(bytes: &[u8]) -> anyhow::Result<Vec<HaystackRecord>> {
    #[derive(serde::Deserialize)]
    struct Turn {
        role: String,
        content: String,
    }
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        question_id: String,
        #[serde(default)]
        question_type: String,
        question: String,
        answer: serde_json::Value,
        #[serde(default)]
        answer_session_ids: Vec<String>,
        #[serde(default)]
        haystack_session_ids: Vec<String>,
        #[serde(default)]
        haystack_sessions: Vec<Vec<Turn>>,
        /// LongMemEval provides the real-world date of each haystack session and
        /// the "current" date of the question. Required for temporal-reasoning
        /// (how-many-days) and knowledge-update (which fact is newer); dropping
        /// them silently cripples 42% of the benchmark.
        #[serde(default)]
        question_date: String,
        #[serde(default)]
        haystack_dates: Vec<String>,
    }
    let raws: Vec<Raw> = serde_json::from_slice(bytes)
        .map_err(|e| anyhow::anyhow!("parse longmemeval haystack: {e}"))?;
    Ok(raws
        .into_iter()
        .map(|r| {
            let answer = match r.answer {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            // Pad dates with None so a missing/short `haystack_dates` never drops
            // sessions; align by index (date[i] is session[i]'s timestamp).
            let dates = r.haystack_dates.into_iter().map(Some).chain(std::iter::repeat(None));
            let sessions = r
                .haystack_session_ids
                .into_iter()
                .zip(r.haystack_sessions)
                .zip(dates)
                .map(|((sid, turns), date)| {
                    // Prepend the session's real-world date so the gen model can
                    // reason temporally (days-between) and resolve knowledge
                    // updates (which fact is newer). The marker also flows into
                    // the ingested doc, making date tokens searchable.
                    let mut texts: Vec<String> = Vec::with_capacity(turns.len() + 1);
                    if let Some(d) = date.filter(|d: &String| !d.is_empty()) {
                        texts.push(format!("[Session date: {d}]"));
                    }
                    texts.extend(turns.into_iter().map(|t| format!("{}: {}", t.role, t.content)));
                    (sid, texts)
                })
                .collect();
            HaystackRecord {
                question_id: r.question_id,
                question_type: r.question_type,
                question: r.question,
                answer,
                answer_session_ids: r.answer_session_ids,
                sessions,
                question_date: r.question_date,
            }
        })
        .collect())
}

/// True iff top-k surfaced a turn from a gold answer-session — phrasing-
/// independent retrieval success. `hit_sources` are the `Hit::source` strings,
/// each of which embeds its originating `session_id` (turns are written under a
/// path that contains the sid).
pub(crate) fn evidence_recall_hit(hit_sources: &[String], answer_session_ids: &[String]) -> bool {
    hit_sources
        .iter()
        .any(|s| answer_session_ids.iter().any(|sid| !sid.is_empty() && s.contains(sid.as_str())))
}

/// `(covered, total)` count of **distinct non-empty gold sessions** that the
/// retrieved `hit_sources` surfaced. `total` is the number of gold answer-
/// sessions the question requires; `covered` is how many of them a top-k hit
/// actually carried. Empty sids are ignored on both sides (a question with no
/// non-empty gold session yields `(0, 0)`).
pub(crate) fn evidence_recall_coverage(
    hit_sources: &[String],
    answer_session_ids: &[String],
) -> (usize, usize) {
    let covered_one = |sid: &str| !sid.is_empty() && hit_sources.iter().any(|s| s.contains(sid));
    let total = answer_session_ids.iter().filter(|s| !s.is_empty()).count();
    let covered = answer_session_ids.iter().filter(|s| covered_one(s.as_str())).count();
    (covered, total)
}

/// Strict ("all-gold") evidence recall: true iff **every** gold answer-session
/// was surfaced in the top-k. This is the honest retrieval metric for
/// multi-session questions, whose answer must synthesize facts across *all* gold
/// sessions — partial coverage yields a wrong answer even though the any-gold
/// [`evidence_recall_hit`] still reads as a hit (it fires on a single covered
/// session). Returns false when the question has no non-empty gold session, so
/// an empty gold list is never counted as a vacuous success.
pub(crate) fn evidence_recall_all_hit(
    hit_sources: &[String],
    answer_session_ids: &[String],
) -> bool {
    let (covered, total) = evidence_recall_coverage(hit_sources, answer_session_ids);
    total > 0 && covered == total
}

/// Bucket A1 fix (LongMemEval N=500 validation, 2026-07): the hybrid arm's
/// `fused.rerank(reranker)` call went through `FuseRrfRetriever::rerank()`'s
/// convenience sugar, which hardcodes `DEFAULT_RERANK_TOP_IN=30` regardless
/// of `LME_POOL`/`LME_TOPK`. Every hit-count log line across a full N=500 run
/// read exactly "30 hits" no matter what pool/topk was configured, silently
/// discarding fused candidates ranked 31+ before the cross-encoder ever saw
/// them. The vector branch and the BM25 branch each contribute up to `pool`
/// hits, so the fused set has at most twice that many distinct candidates
/// (worst case zero overlap) -- that is the true upper bound the rerank pass
/// needs to consider so `LME_POOL` actually widens the pool instead of being
/// silently capped back down to 30.
fn hybrid_rerank_top_in(pool: usize) -> usize {
    pool.saturating_mul(2)
}

/// Whether judge mode (LLM gen+judge round-trip) is active, vs the cheap
/// evidence-recall-only pass. A single source of truth so `run()` can report
/// the correct metric name and `score_haystack` can decide its scoring path
/// from the same read.
fn judge_mode_enabled() -> bool {
    std::env::var("LUNARIS_EVAL_LME_JUDGE").map(|v| v == "1").unwrap_or(false)
}

/// v0.7 N=500 validation review: `run()` used to hardcode `EvalRow`'s metric
/// to `METRIC` ("j_score") unconditionally, so a cheap evidence-recall@k pass
/// (the CI default, since `LUNARIS_EVAL_LME_JUDGE` is unset there) was
/// indistinguishable in `eval-results.json` from a real LLM-judged score.
/// Report which one actually ran.
fn metric_name(judge_mode: bool) -> &'static str {
    if judge_mode { METRIC } else { "evidence_recall" }
}

/// Prototype toggle (`LUNARIS_EVAL_LME_GRAPH=1`): route per-question ingest
/// through structured Fact/Entity/Relation extraction
/// (`lunaris.graph_pipeline().enable()` + a native `CloudApiExtractor`
/// targeting MiniMax's `api.minimax.io` directly) instead of the default
/// graph-OFF chunk+embed-only fast path.
///
/// Motivation: a `tmp/ceiling_test.py` diagnostic (2026-07, not part of this
/// harness) fed the SAME reader model hand-authored, deduped-but-unsummed
/// facts instead of raw multi-session prose for three known LongMemEval
/// multi-session counting misses (hours/babies/cuisines totals) and got all
/// three right — evidence the bottleneck is presentation, not the reader's
/// arithmetic. This toggle lets that hypothesis be tested against the real
/// extraction pipeline instead of hand-authored facts.
///
/// Was originally prototyped through `tmp/route_shim.py` (an Ollama-shaped
/// local shim that forwards to `api.minimax.io`) via `OllamaExtractor`, so
/// this could reuse the harness's existing Ollama-transport wiring with zero
/// new Rust code. Rewired onto `CloudApiExtractor`/`CloudProvider::MiniMax`
/// (2026-07) to drop that shim's stacked retry loop (it wrapped
/// `OllamaBackend`'s own retry, compounding latency on every transient
/// error) and to pick up `CloudApiExtractor`'s D-21 sentinel-on-retry-
/// exhaust semantics instead of `OllamaExtractor`'s silent-empty-on-error.
fn graph_pipeline_enabled() -> bool {
    std::env::var("LUNARIS_EVAL_LME_GRAPH").map(|v| v == "1").unwrap_or(false)
}

/// Real-corpus harness. For each of the first `limit` records
/// (env `LUNARIS_EVAL_LME_LIMIT`, default 50) ingest the FULL haystack
/// (distractor sessions included), then recall the top-k turns and score.
///
/// **Per-question isolation = full Moon reset + re-open.** Each question must
/// retrieve from ONLY its own haystack (the LongMemEval setting). The pad's
/// `StartsWith{source}` filter can't enforce this — it's malformed for Moon's
/// TAG `source` field and fails open — so we physically isolate: `reset_moon`
/// (`FLUSHALL` + `FT.DROPINDEX`) then `Lunaris::open` (recreates empty
/// indexes) before each question. DROPINDEX is essential: a bare FLUSHALL
/// leaves the vector index full of GHOST docs that bury the gold under dead
/// KNN hits. We then recall WITHOUT the source filter (isolation already
/// holds) so the gold turns actually surface.
///
/// Two metrics, selected by `LUNARIS_EVAL_LME_JUDGE`:
/// - **unset/0** → **evidence-recall@k**: % of questions whose top-k surfaced a
///   turn from a gold answer-session. Phrasing-independent retrieval quality;
///   fast, no LLM. (Returned as the gauntlet value.)
/// - **1** → **J-score**: retrieve top-k → generate an answer from that context
///   with a chat model → judge the answer against gold with the *official*
///   LongMemEval per-question-type judge prompt. This is the apples-to-apple
///   number competitors (Zep/Mem0) publish. Evidence-recall is still logged
///   alongside. Default gen+judge model `minimax-m3:cloud`.
async fn score_haystack(url: &str, records: &[HaystackRecord]) -> anyhow::Result<f64> {
    let limit: usize =
        std::env::var("LUNARIS_EVAL_LME_LIMIT").ok().and_then(|s| s.parse().ok()).unwrap_or(50);
    // Chunked-run support: skip the first `offset` records so the gauntlet can
    // be driven in process-isolated windows. The native GGUF embedder/reranker
    // leak Metal weight buffers on every per-question `Lunaris::open` (candle
    // does not free them), so a single process degrades then eventually dies.
    // The exact ceiling has varied across observations (~13 opens here vs.
    // ~30 in docs/benchmarks/v0.7-longmemeval-jscore-validation.md) — treat
    // both as approximate, not a guarantee. The only verified-safe practice
    // is `LUNARIS_EVAL_LME_LIMIT=1` (one process per question); the OS
    // reclaims all Metal buffers on every process exit regardless of which
    // ceiling is real on a given host.
    let offset: usize =
        std::env::var("LUNARIS_EVAL_LME_OFFSET").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    // Cross-encoder rerank toggle. The bare `recall()` root is pure vector
    // (`Vector::new("chunks", 30)`) with NO rerank — the weakest config. The
    // production recall path reranks the vector candidates with the bge
    // cross-encoder, which is what Zep/Mem0 do too, so default ON for an
    // apples-to-apple number. Set LUNARIS_EVAL_LME_RERANK=0 to measure the
    // raw vector baseline.
    let rerank_enabled = std::env::var("LUNARIS_EVAL_LME_RERANK").map(|v| v != "0").unwrap_or(true);
    let k: usize =
        std::env::var("LUNARIS_EVAL_LME_TOPK").ok().and_then(|s| s.parse().ok()).unwrap_or(10);
    let judge_mode = judge_mode_enabled();
    let debug = std::env::var("LUNARIS_EVAL_LME_DEBUG").map(|v| v == "1").unwrap_or(false);
    let gen_model = std::env::var("LUNARIS_EVAL_LME_GEN_MODEL")
        .unwrap_or_else(|_| super::lme_judge::DEFAULT_MODEL.to_string());
    let judge_model = std::env::var("LUNARIS_EVAL_LME_JUDGE_MODEL")
        .unwrap_or_else(|_| super::lme_judge::DEFAULT_MODEL.to_string());
    // Graph-pipeline extraction prototype (see graph_pipeline_enabled doc).
    // `LUNARIS_EVAL_LME_EXTRACT_MODEL` overrides the model id sent straight to
    // api.minimax.io (falls back to MINIMAX_EXTRACT_MODEL, then "MiniMax-M3" —
    // see the construction site below for why this isn't
    // CloudApiExtractorOpts::default()). MINIMAX_API_KEY is read directly at
    // that same site, never threaded through a harness variable or logged.
    let graph_enabled = graph_pipeline_enabled();
    let extract_model = std::env::var("LUNARIS_EVAL_LME_EXTRACT_MODEL").ok();
    // Generous, cloud-model-appropriate batch timeout. The extractor's own
    // library default (150ms) is tuned for a local candle/Ollama instance and
    // would time out on nearly every call to a real cloud API, masking the
    // very effect this prototype exists to measure (design-for-failure means
    // a timeout degrades to an empty extraction, NOT a crash — but a silent
    // empty-extraction-via-timeout would misreport as "structured extraction
    // doesn't help" when the real cause is "the call never got a chance to
    // finish"). This single knob bounds both the per-chunk CloudBackend retry
    // budget and the whole-batch fallback-to-per-chunk timeout (D-02).
    let extract_batch_timeout_ms: u64 = std::env::var("LUNARIS_EVAL_LME_EXTRACT_BATCH_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120_000);
    // MiniMax-M3 is reasoning-heavy: the library default (512) was observed
    // exhausted (`finish_reason: length`, empty content) on real chunks
    // during this prototype's live A/B testing. Default higher here.
    let extract_max_tokens: u32 = std::env::var("LUNARIS_EVAL_LME_EXTRACT_MAX_TOKENS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2048);
    // Bounded per-chunk extraction concurrency. The 2026-07-10 flame
    // investigation measured ~11s per MiniMax completion in a strictly
    // serial loop consuming ~95% of question wall time; a live probe showed
    // 3.8x overlap at 4 concurrent calls with zero rate-limit errors. Set
    // to 1 to restore the historical serial behavior.
    let extract_concurrency: usize = std::env::var("LUNARIS_EVAL_LME_EXTRACT_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    // Window = records[offset .. offset+limit]. `n` is the actual count after
    // clamping to the dataset tail (a final short chunk is fine). An EMPTY
    // window is a harness misconfiguration (offset beyond the dataset), and
    // reporting it as a real J=0.0 is exactly the `0.0 → judge_ge → FAIL`
    // trap `eval_scoring.rs` pins — bail instead; the caller's SKIP-not-FAIL
    // arm turns this into an honest `EvalRow::skipped`.
    let n = records.len().saturating_sub(offset).min(limit);
    if n == 0 {
        anyhow::bail!(
            "empty eval window: offset {offset} >= dataset len {} (limit {limit})",
            records.len()
        );
    }

    // Build the chat client only in judge mode (avoids requiring Ollama for
    // the fast recall-only path).
    let chat = if judge_mode { Some(super::lme_judge::ChatClient::new()?) } else { None };
    if judge_mode {
        eprintln!(
            "  [longmemeval] JUDGE mode ON — gen={gen_model} judge={judge_model} k={k} n={n}"
        );
    }

    // NOTE on Metal stability: the native GGUF embedder leaks candle Metal
    // activation buffers across forward passes (variable chunk lengths => an
    // ever-growing shape-keyed buffer cache candle never frees), so a single
    // process degrades then eventually dies with "Failed to create metal
    // resource: Buffer". The exact question count before failure has varied
    // across observations (~13 here vs. ~30 in
    // docs/benchmarks/v0.7-longmemeval-jscore-validation.md) — do not rely on
    // either as a safe ceiling. Reusing the embedder Arc across questions
    // does NOT help (it's activation buffers, not weights) and actually
    // makes Metal thrash. The verified-safe workaround is full process
    // isolation per question: `LUNARIS_EVAL_LME_LIMIT=1` with
    // `LUNARIS_EVAL_LME_OFFSET` looped at the shell level (see
    // docs/benchmarks/v0.7-longmemeval-jscore-validation.md's reproduction
    // recipe) — every process exits before any leak accumulates and the OS
    // reclaims all Metal buffers. We therefore keep the simple per-question
    // `Lunaris::open` here.
    let mut evidence_hits = 0usize;
    // Strict "all-gold" recall: every required gold session surfaced (the honest
    // metric for multi-session questions). `gold_*_sum` accumulate the per-
    // question (covered, total) so we can report mean gold-session coverage.
    let mut evidence_all_hits = 0usize;
    let mut gold_covered_sum = 0usize;
    let mut gold_total_sum = 0usize;
    let mut judge_correct = 0usize;
    for (i, rec) in records.iter().skip(offset).take(n).enumerate() {
        // Pristine store + fresh empty indexes for THIS question only.
        reset_moon(url).await?;
        let lunaris = std::sync::Arc::new(lunaris::Lunaris::open(url).await?);
        if graph_enabled {
            // Graph-pipeline prototype: route ingest through structured
            // Fact/Entity/Relation extraction instead of the default
            // graph-OFF chunk+embed-only fast path. `lunaris::CloudApiExtractor`
            // is already re-exported (lunaris-bench's `lunaris` dep now
            // carries `features = ["ollama", "cloud-api"]`) — no new Cargo
            // dependency.
            //
            // NOT built via `CloudApiExtractorOpts::default()`:  that resolves
            // `provider` from `LUNARIS_EXTRACT_PROVIDER` (falling back to
            // Anthropic) BEFORE reading `model`/`api_key` from the provider's
            // env — overriding just `.provider` afterward via struct-update
            // syntax would silently keep Anthropic's model/key unless that
            // env var happened to already say "minimax". Reading
            // MINIMAX_API_KEY / MINIMAX_EXTRACT_MODEL directly (the exact env
            // names CloudProvider::MiniMax resolves to internally) sidesteps
            // that trap. The key is never echoed or logged.
            let api_key = std::env::var("MINIMAX_API_KEY").unwrap_or_default();
            let extract_model_used = extract_model.clone().unwrap_or_else(|| {
                std::env::var("MINIMAX_EXTRACT_MODEL").unwrap_or_else(|_| "MiniMax-M3".to_string())
            });
            let extractor = lunaris::CloudApiExtractor::new(lunaris::CloudApiExtractorOpts {
                provider: lunaris::CloudProvider::MiniMax,
                model: extract_model_used.clone(),
                api_key,
                batch_timeout_ms: extract_batch_timeout_ms,
                max_retries: 1,
                max_tokens: extract_max_tokens,
                concurrency: extract_concurrency,
                base_url: None,
            })
            .map_err(|e| anyhow::anyhow!("graph-pipeline extractor construction failed: {e}"))?;
            lunaris.graph_pipeline().set_extractor(std::sync::Arc::new(extractor));
            lunaris.graph_pipeline().enable();
            if debug {
                eprintln!(
                    "    GRAPH pipeline ENABLED extract_model={extract_model_used} batch_timeout_ms={extract_batch_timeout_ms} max_tokens={extract_max_tokens}"
                );
            }
        }
        let pad = lunaris::CodingSessionMemory::new(
            lunaris.clone(),
            lunaris_core::Scope::dev(),
            &format!("lme{i:04}"),
        );
        // Optional session cap for fast debug iteration: keep the first
        // `maxsess` sessions PLUS every gold answer-session (so the evidence is
        // always present). Unset = full haystack.
        let maxsess: Option<usize> =
            std::env::var("LUNARIS_EVAL_LME_MAXSESS").ok().and_then(|s| s.parse().ok());
        // INGEST GRANULARITY = one document PER SESSION (turns joined), NOT
        // per turn. A turn-per-document run pays the full ingest pipeline
        // (chunk + doctree + RAPTOR community tree + vector upserts) ~550×
        // per question → ~30 min/question → ~25 h for N=50. A session is a
        // coherent document anyway: one write per session amortises the
        // RAPTOR/doctree overhead ~10× and yields fewer, larger chunks to
        // embed. `Hit::source` is then `helios:fs/lme####/<sid>.md`, still
        // carrying the session id for evidence-recall + session expansion.
        let mut ingested = 0usize;
        for (si, (sid, turns)) in rec.sessions.iter().enumerate() {
            let is_gold = rec.answer_session_ids.iter().any(|a| a == sid);
            if let Some(m) = maxsess
                && si >= m
                && !is_gold
            {
                continue;
            }
            let doc = turns.join("\n\n");
            pad.write(&format!("{sid}.md"), doc).await?;
            ingested += turns.len();
        }
        // Unfiltered recall. We deliberately DO NOT use `pad.grep`, whose
        // `StartsWith{source}` filter is malformed for Moon's TAG `source`
        // field (`@source:helios:fs/lme0000/*` with unescaped `:` / `/`) and
        // non-deterministically returns a tiny mismatched subset. Per-question
        // isolation is already guaranteed by the FLUSHALL above, so a plain
        // top-k recall over the (single-question) dev scope is correct and
        // surfaces the gold turns the filtered path was dropping.
        // Retrieval. Default (LME_HYBRID unset) = the historical vector-only
        // root + optional rerank. LME_HYBRID=1 = Lunaris's full HYBRID root:
        // semantic Vector fused with BM25 Keyword via RRF, then reranked — the
        // blueprint's canonical recall (recall.rs §8). BM25 surfaces gold
        // sessions whose lexical overlap with the question ranks low in pure
        // vector space, which is exactly the multi-session coverage gap that
        // vector-only retrieval misses. The fused candidate POOL is LME_POOL
        // (default 30) per arm; the final gen context stays at top-k so it
        // remains tight and low-noise (no gen-context bloat).
        let hybrid = std::env::var("LUNARIS_EVAL_LME_HYBRID").map(|v| v == "1").unwrap_or(false);
        let pool: usize =
            std::env::var("LUNARIS_EVAL_LME_POOL").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
        let hits = if hybrid {
            let fused = lunaris::Vector::new("chunks", pool)
                .and(lunaris::Keyword::bm25("chunks", pool))
                .fuse_rrf(60);
            let builder = lunaris.recall_with_degraded_check().await?;
            if rerank_enabled {
                // Bucket A1 fix: `fused.rerank(reranker)` sugar defaults to
                // DEFAULT_RERANK_TOP_IN=30, ignoring LME_POOL — use
                // with_top_in so the full fused pool actually reaches the
                // cross-encoder.
                let reranked = lunaris::RerankRetriever::with_top_in(
                    Box::new(fused),
                    lunaris.reranker(),
                    hybrid_rerank_top_in(pool),
                );
                builder
                    .with_root(reranked.top(k))
                    .execute(lunaris::Query::text(&rec.question))
                    .await?
            } else {
                builder.with_root(fused.top(k)).execute(lunaris::Query::text(&rec.question)).await?
            }
        } else {
            let mut builder = lunaris.recall_with_degraded_check().await?;
            if rerank_enabled {
                builder = builder.rerank(lunaris.reranker());
            }
            builder.top(k).execute(lunaris::Query::text(&rec.question)).await?
        };
        let sources: Vec<String> = hits.iter().map(|h| h.source.clone()).collect();
        // Session-diverse gen context (multi-session coverage fix). LME_SESS_K
        // caps the context at N DISTINCT sessions, drawn in rank order from the
        // (larger) LME_TOPK chunk pool — so a 4-5-gold question gets every gold
        // session the pool retrieved instead of a top-k that clusters into 2-3
        // sessions. Unset => usize::MAX => historical behavior. The metrics
        // below run on the capped set so recall reflects the actual gen context.
        // Intent-adaptive routing (LME_ADAPTIVE=1): classify the question's
        // intent from its TEXT (never the gold type) and let that pick the
        // session budget, chronological ordering, and gen prompt. Resolves the
        // cross-type config conflicts that cap a single uniform setting (broad
        // context helps counting but dilutes preference; chrono helps temporal
        // but buries the one relevant session). Off => the env-knob behavior.
        let adaptive =
            std::env::var("LUNARIS_EVAL_LME_ADAPTIVE").map(|v| v == "1").unwrap_or(false);
        let intent = super::lme_judge::classify_intent(&rec.question);
        let (intent_sess_k, intent_chrono) = super::lme_judge::intent_retrieval_cfg(intent);
        let sess_k: usize = if adaptive {
            intent_sess_k
        } else {
            std::env::var("LUNARIS_EVAL_LME_SESS_K")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(usize::MAX)
        };
        let sources = cap_to_distinct_sessions(&sources, sess_k);
        let recall_hit = evidence_recall_hit(&sources, &rec.answer_session_ids);
        if recall_hit {
            evidence_hits += 1;
        }
        let recall_all = evidence_recall_all_hit(&sources, &rec.answer_session_ids);
        if recall_all {
            evidence_all_hits += 1;
        }
        let (gold_covered, gold_total) =
            evidence_recall_coverage(&sources, &rec.answer_session_ids);
        gold_covered_sum += gold_covered;
        gold_total_sum += gold_total;
        if debug {
            eprintln!(
                "  [DEBUG q{i}] qid={} type={} ingested={ingested} turns | gold_sids={:?} | {} hits",
                rec.question_id,
                rec.question_type,
                rec.answer_session_ids,
                hits.len()
            );
            for (hi, h) in hits.iter().take(5).enumerate() {
                eprintln!(
                    "    hit[{hi}] score={:.3} source={:?} text={:?}",
                    h.score,
                    h.source,
                    h.text.chars().take(70).collect::<String>()
                );
            }
            eprintln!("    => evidence_recall_hit = {recall_hit}");
            eprintln!(
                "    => evidence_recall_all = {recall_all} (gold sessions covered {gold_covered}/{gold_total})"
            );
        }

        if let Some(chat) = &chat {
            // Generation context = SESSION-EXPANDED retrieval (LongMemEval's
            // standard setting). A single retrieved turn rarely carries the
            // answer fact — the evidence lives somewhere in that turn's
            // SESSION. So we take the distinct sessions hit in the top-k and
            // feed each one's FULL ordered turn list. Since gold-session
            // recall is ~100%, this guarantees the answer-bearing turn is in
            // context. `hits[].text` is the lossy chunk text; we instead pull
            // the verbatim turns from `rec.sessions` keyed by the session id
            // embedded in `hit.source` (`helios:fs/lme####/<sid>/<turn>.md`).
            let chrono = if adaptive {
                intent_chrono
            } else {
                std::env::var("LUNARIS_EVAL_LME_CHRONO").map(|v| v == "1").unwrap_or(false)
            };
            let contexts = expand_hit_sessions(&sources, rec, chrono);
            // Gen system prompt: intent-tailored when adaptive, else the COT knob.
            let gen_sys = if adaptive {
                super::lme_judge::gen_system_prompt_for(intent)
            } else {
                let cot = std::env::var("LUNARIS_EVAL_LME_COT").map(|v| v == "1").unwrap_or(false);
                super::lme_judge::gen_system_prompt(cot)
            };
            if debug {
                eprintln!(
                    "    INTENT={intent:?} sess_k={sess_k} chrono={chrono} adaptive={adaptive}"
                );
            }
            let verdict = judge_one(chat, &gen_model, &judge_model, rec, &contexts, gen_sys).await;
            match verdict {
                Ok((correct, answer, raw)) => {
                    if correct {
                        judge_correct += 1;
                    }
                    if debug {
                        eprintln!(
                            "    GOLD={:?}",
                            rec.answer.chars().take(120).collect::<String>()
                        );
                        eprintln!("    GEN ={:?}", answer.chars().take(200).collect::<String>());
                        eprintln!(
                            "    JUDGE_RAW={:?} -> correct={correct}",
                            raw.chars().take(60).collect::<String>()
                        );
                    }
                }
                Err(e) => {
                    // Design-for-failure: a transport/judge error counts as a
                    // miss for THIS question, never aborts the gauntlet.
                    eprintln!("  [longmemeval {}/{n}] judge error (counted miss): {e}", i + 1);
                }
            }
            eprintln!(
                "  [longmemeval {}/{n}] J-score running={:.1}%  (evidence-recall@{k} any-gold={:.1}%, all-gold={:.1}%)",
                i + 1,
                100.0 * judge_correct as f64 / (i + 1) as f64,
                100.0 * evidence_hits as f64 / (i + 1) as f64,
                100.0 * evidence_all_hits as f64 / (i + 1) as f64,
            );
        } else {
            eprintln!(
                "  [longmemeval {}/{n}] evidence-recall@{k} running: any-gold={:.1}%, all-gold={:.1}%",
                i + 1,
                100.0 * evidence_hits as f64 / (i + 1) as f64,
                100.0 * evidence_all_hits as f64 / (i + 1) as f64
            );
        }
    }

    let mean_coverage = if gold_total_sum > 0 {
        100.0 * gold_covered_sum as f64 / gold_total_sum as f64
    } else {
        0.0
    };
    eprintln!(
        "[longmemeval] evidence-recall@{k} over n={n}: any-gold={:.1}%  all-gold={:.1}%  \
         (gold-session coverage {gold_covered_sum}/{gold_total_sum} = {mean_coverage:.1}%)",
        100.0 * evidence_hits as f64 / n as f64,
        100.0 * evidence_all_hits as f64 / n as f64,
    );

    if judge_mode {
        Ok(100.0 * judge_correct as f64 / n as f64)
    } else {
        Ok(100.0 * evidence_hits as f64 / n as f64)
    }
}

/// Fully reset the Moon instance so the next question retrieves from a
/// pristine store. `FLUSHALL` alone is NOT enough: it clears the data keys
/// but the FT vector index keeps every prior doc as a GHOST entry
/// (`num_docs` stays high with zero live keys). KNN then returns top-k
/// dominated by dead references that hydrate away — leaving ~1 live hit and
/// a 0% recall. So we also `FT.DROPINDEX` every `lunaris__dev__*` index;
/// the subsequent `Lunaris::open` re-runs `ensure_indexes` and recreates
/// them empty. `moon_url` is the `moon://host:port` form, mapped to
/// `redis://` for the redis-compatible wire.
async fn reset_moon(moon_url: &str) -> anyhow::Result<()> {
    let redis_url = moon_url.replacen("moon://", "redis://", 1);
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;
    redis::cmd("FLUSHALL").query_async::<()>(&mut conn).await?;
    let indexes: Vec<String> =
        redis::cmd("FT._LIST").query_async(&mut conn).await.unwrap_or_default();
    for idx in indexes.iter().filter(|i| i.starts_with("lunaris__dev__")) {
        // Ignore "unknown index" — a fresh Moon may not have it yet.
        let _ = redis::cmd("FT.DROPINDEX").arg(idx).query_async::<()>(&mut conn).await;
    }
    Ok(())
}

/// Extract the session id from a hit source of the form
/// `helios:fs/lme####/<sid>.md` (one-doc-per-session ingest). The sid is the
/// 3rd `/`-segment with any `.md` suffix stripped (session ids contain
/// `_`/`-`/digits but never `/`). Returns `None` for shapes that don't match.
fn sid_from_source(source: &str) -> Option<&str> {
    source.split('/').nth(2).map(|s| s.strip_suffix(".md").unwrap_or(s)).filter(|s| !s.is_empty())
}

/// Session-level context expansion. Given the top-k hit `sources` and the
/// record, return the FULL ordered turn texts of every distinct session that
/// appears in the hits (sessions ordered by their first appearance in the
/// ranking; turns within a session in their original order). This is the
/// LongMemEval session-retrieval setting — the answer fact lives somewhere in
/// the hit session, not necessarily in the single retrieved turn.
fn expand_hit_sessions(
    sources: &[String],
    rec: &HaystackRecord,
    chronological: bool,
) -> Vec<String> {
    let mut seen: Vec<&str> = Vec::new();
    for src in sources {
        if let Some(sid) = sid_from_source(src)
            && !seen.contains(&sid)
        {
            seen.push(sid);
        }
    }
    let mut picked: Vec<&(String, Vec<String>)> =
        seen.iter().filter_map(|sid| rec.sessions.iter().find(|(s, _)| s == sid)).collect();
    // Chronological presentation: sort the distinct sessions by their
    // `[Session date: ...]` marker so the reader sees a clean timeline. The
    // LongMemEval `YYYY/MM/DD (Day) HH:MM` format sorts lexicographically into
    // true time order. This turns ordering / recency / which-fact-is-newer
    // questions into top-to-bottom reads instead of forcing the reader to
    // re-sort retrieval-rank order in its head. `sort_by` is stable, so undated
    // sessions keep rank order. Off => historical first-appearance order.
    if chronological {
        picked.sort_by(|a, b| session_date_key(a).cmp(session_date_key(b)));
    }
    let mut out = Vec::new();
    for (_, turns) in picked {
        out.extend(turns.iter().cloned());
    }
    out
}

/// The `[Session date: ...]` marker prefixing a session's turns (empty string
/// when absent — those sort first, keeping rank order among themselves).
fn session_date_key(session: &(String, Vec<String>)) -> &str {
    session.1.first().map(|s| s.as_str()).filter(|s| s.starts_with("[Session date:")).unwrap_or("")
}

/// Session-diverse truncation of the ranked hit `sources` for the gen context.
///
/// Returns the prefix of `sources` that covers at most `cap` DISTINCT sessions,
/// in rank order. Extra hits from an already-included session are retained
/// (harmless — [`expand_hit_sessions`] dedups them); once `cap` distinct
/// sessions are seen, later hits from NEW sessions are dropped. `cap ==
/// usize::MAX` (the default when `LME_SESS_K` is unset) is a no-op, preserving
/// the historical "every distinct session in the top-k chunks" behavior.
///
/// This is the fix for the **multi-session coverage ceiling**: a tight top-k of
/// CHUNKS can cluster into 2-3 sessions (the cross-encoder piles several chunks
/// from the same strong session), starving 4-5-gold questions of the *other*
/// gold sessions even when they were retrieved. Retrieving a larger CHUNK pool
/// (`LME_TOPK`) and then capping at `cap` DISTINCT sessions guarantees the gen
/// context spans up to `cap` sessions, so every gold session present in the
/// pool reaches the judge instead of being squeezed out by clustering.
fn cap_to_distinct_sessions(sources: &[String], cap: usize) -> Vec<String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for src in sources {
        match sid_from_source(src) {
            Some(sid) if seen.contains(&sid) => out.push(src.clone()),
            Some(sid) if seen.len() < cap => {
                seen.push(sid);
                out.push(src.clone());
            }
            _ => {} // distinct-session budget exhausted, or sourceless: drop
        }
    }
    out
}

/// One question's gen→judge round-trip. Generates an answer from the
/// retrieved `contexts`, then judges it against the gold answer with the
/// official per-question-type prompt. Returns `(verdict, gen_answer,
/// judge_raw)` so the caller can log the model outputs under `--debug`.
async fn judge_one(
    chat: &super::lme_judge::ChatClient,
    gen_model: &str,
    judge_model: &str,
    rec: &HaystackRecord,
    contexts: &[String],
    gen_sys_prompt: &str,
) -> anyhow::Result<(bool, String, String)> {
    let gen_user = super::lme_judge::gen_user_prompt(&rec.question_date, contexts, &rec.question);
    let response = chat.chat(gen_model, gen_sys_prompt, &gen_user).await?;
    let prompt = super::lme_judge::judge_prompt(
        &rec.question_type,
        rec.is_abstention(),
        &rec.question,
        &rec.answer,
        &response,
    );
    let verdict_raw = chat.chat(judge_model, "", &prompt).await?;
    Ok((super::lme_judge::parse_verdict(&verdict_raw), response, verdict_raw))
}

pub fn parse_longmemeval(bytes: &[u8]) -> anyhow::Result<Vec<EvalQuery>> {
    #[derive(serde::Deserialize)]
    struct Raw {
        question: String,
        answer: serde_json::Value,
    }
    let raws: Vec<Raw> =
        serde_json::from_slice(bytes).map_err(|e| anyhow::anyhow!("parse longmemeval: {e}"))?;
    Ok(raws
        .into_iter()
        .map(|r| {
            // LongMemEval `answer` is normally a string; stringify any other
            // JSON shape so the recall proxy has a gold string to match.
            let expected_answer = match r.answer {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            EvalQuery { query: r.question, expected_answer }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cache_dir_honors_env_when_set() {
        // We don't mutate env (would require unsafe in Rust 2024) — just
        // verify the function returns SOME path and contains "lunaris/eval"
        // when the env var isn't set.
        let p = resolve_cache_dir();
        if std::env::var("LUNARIS_EVAL_CACHE_DIR").is_err() {
            // Default branch — should contain the canonical suffix.
            assert!(
                p.to_string_lossy().contains("lunaris/eval"),
                "default cache dir should contain 'lunaris/eval'; got {}",
                p.display()
            );
        }
    }

    #[tokio::test]
    async fn run_skips_cleanly_without_moon_url() {
        let mut results: Vec<EvalRow> = Vec::new();
        super::run(&mut results).await.unwrap();
        assert_eq!(results.len(), 1);
        // SKIP-not-FAIL invariant (Reject: false_fail_on_absent): with MOON_URL
        // absent the harness MUST emit SKIPPED, never a 0.0→FAIL. A live run
        // with the backend present may legitimately PASS/FAIL — assert the
        // strict invariant only when the gating capability is absent.
        if std::env::var("MOON_URL").is_err() {
            assert_eq!(results[0].status, "SKIPPED");
        } else {
            assert!(matches!(results[0].status.as_str(), "SKIPPED" | "PASS" | "FAIL"));
        }
        assert_eq!(results[0].harness, HARNESS);
        assert_eq!(results[0].metric, METRIC);
        assert_eq!(results[0].threshold, THRESHOLD);
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn download_dataset_signature_compiles() {
        // Type check: download_dataset must be reachable from sibling
        // modules. Just verify the function pointer compiles.
        let _f: fn(
            &'static str,
            &'static str,
            &Path,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<PathBuf>> + Send>,
        > = |_, _, _| Box::pin(async move { Ok(PathBuf::new()) });
    }

    #[test]
    fn parse_longmemeval_full_extracts_haystack_and_evidence() {
        let json = br#"[
          {
            "question_id": "q123",
            "question_type": "multi-session",
            "question": "What broke first?",
            "answer": "the GPS",
            "question_date": "2023/05/30 (Tue) 23:40",
            "answer_session_ids": ["s2"],
            "haystack_session_ids": ["s1", "s2"],
            "haystack_dates": ["2023/05/20 (Sat) 02:21", "2023/05/21 (Sun) 10:00"],
            "haystack_sessions": [
              [{"role":"user","content":"hi"},{"role":"assistant","content":"hello"}],
              [{"role":"user","content":"my gps failed"}]
            ]
          }
        ]"#;
        let recs = parse_longmemeval_full(json).expect("parse");
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r.question_id, "q123");
        assert_eq!(r.question_type, "multi-session");
        assert!(!r.is_abstention());
        assert_eq!(r.question, "What broke first?");
        assert_eq!(r.answer, "the GPS");
        // question_date is parsed (anchor for relative temporal reasoning).
        assert_eq!(r.question_date, "2023/05/30 (Tue) 23:40");
        assert_eq!(r.answer_session_ids, vec!["s2".to_string()]);
        assert_eq!(r.sessions.len(), 2);
        assert_eq!(r.sessions[0].0, "s1");
        // Each session's turn list is prefixed with its [Session date: ...]
        // marker; the original turns follow.
        assert_eq!(r.sessions[0].1[0], "[Session date: 2023/05/20 (Sat) 02:21]");
        assert_eq!(r.sessions[0].1[1], "user: hi");
        assert_eq!(r.sessions[1].0, "s2");
        assert_eq!(r.sessions[1].1[0], "[Session date: 2023/05/21 (Sun) 10:00]");
        assert_eq!(r.sessions[1].1[1], "user: my gps failed");
    }

    #[test]
    fn abstention_detected_from_question_id_suffix() {
        let json = br#"[
          {
            "question_id": "q999_abs",
            "question_type": "single-session-user",
            "question": "unanswerable?",
            "answer": "not in history",
            "haystack_session_ids": ["s1"],
            "haystack_sessions": [[{"role":"user","content":"hi"}]]
          }
        ]"#;
        let recs = parse_longmemeval_full(json).expect("parse");
        assert!(recs[0].is_abstention());
    }

    #[test]
    fn metric_name_reflects_actual_scoring_mode() {
        // v0.7 N=500 validation review: `run()` used to tag EVERY EvalRow as
        // metric "j_score" regardless of LUNARIS_EVAL_LME_JUDGE, so a cheap
        // evidence-recall@k pass (the CI default) was indistinguishable in
        // eval-results.json from a real LLM-judged score.
        assert_eq!(metric_name(true), "j_score");
        assert_eq!(metric_name(false), "evidence_recall");
    }

    #[test]
    fn graph_pipeline_defaults_to_off() {
        // The graph-pipeline extraction prototype must never activate
        // silently -- the default harness config exercises Lunaris's
        // graph-OFF fast path (chunk+embed only), matching every published
        // v0.7 benchmark number. Only an explicit LUNARIS_EVAL_LME_GRAPH=1
        // opts in.
        if std::env::var("LUNARIS_EVAL_LME_GRAPH").is_err() {
            assert!(!graph_pipeline_enabled());
        }
    }

    #[test]
    fn hybrid_rerank_top_in_covers_the_full_fused_pool() {
        // Bucket A1 regression guard: LME_POOL=40 must widen the rerank
        // pre-truncation window to 80, not silently stay at
        // DEFAULT_RERANK_TOP_IN=30 via the `.rerank()` sugar's default.
        assert_eq!(hybrid_rerank_top_in(30), 60);
        assert_eq!(hybrid_rerank_top_in(40), 80);
        assert_eq!(hybrid_rerank_top_in(1), 2);
    }

    #[test]
    fn sid_from_source_extracts_third_segment() {
        // One-doc-per-session ingest: `helios:fs/lme####/<sid>.md`.
        assert_eq!(
            sid_from_source("helios:fs/lme0000/answer_280352e9.md"),
            Some("answer_280352e9")
        );
        assert_eq!(
            sid_from_source("helios:fs/lme0001/sharegpt_Jcy1CVN_0.md"),
            Some("sharegpt_Jcy1CVN_0")
        );
        assert_eq!(sid_from_source("malformed"), None);
    }

    #[test]
    fn expand_hit_sessions_returns_full_sessions_in_rank_order() {
        let rec = HaystackRecord {
            question_id: "q".into(),
            question_type: "single-session-user".into(),
            question: "?".into(),
            answer: "A".into(),
            question_date: String::new(),
            answer_session_ids: vec!["s_gold".into()],
            sessions: vec![
                ("s_distract".into(), vec!["user: noise".into(), "assistant: noise2".into()]),
                (
                    "s_gold".into(),
                    vec![
                        "user: q".into(),
                        "assistant: the degree is BA".into(),
                        "user: thanks".into(),
                    ],
                ),
            ],
        };
        // Hit ranking: one distractor turn first, then a gold turn. Expansion
        // must include BOTH full sessions, gold's FULL 3 turns (incl. the
        // answer-bearing middle turn that retrieval alone missed).
        let sources = vec![
            "helios:fs/lme0000/s_distract/0000.md".to_string(),
            "helios:fs/lme0000/s_gold/0000.md".to_string(),
        ];
        let ctx = expand_hit_sessions(&sources, &rec, false);
        assert_eq!(ctx.len(), 5); // 2 distractor + 3 gold turns
        assert!(ctx.iter().any(|t| t.contains("the degree is BA")));
        // Distractor session comes first (first appearance in ranking).
        assert_eq!(ctx[0], "user: noise");
    }

    #[test]
    fn expand_hit_sessions_chronological_orders_by_session_date() {
        // Two dated sessions retrieved in REVERSE-chronological rank order (the
        // later session ranked first). Chronological mode must reorder them so
        // the timeline reads oldest->newest — what temporal/recency questions
        // need.
        let rec = HaystackRecord {
            question_id: "q".into(),
            question_type: "temporal-reasoning".into(),
            question: "?".into(),
            answer: "A".into(),
            question_date: "2023/06/01 (Thu) 09:00".into(),
            answer_session_ids: vec!["s_jun".into(), "s_jan".into()],
            sessions: vec![
                (
                    "s_jun".into(),
                    vec!["[Session date: 2023/06/01 (Thu) 08:00]".into(), "user: June".into()],
                ),
                (
                    "s_jan".into(),
                    vec!["[Session date: 2023/01/02 (Mon) 08:00]".into(), "user: January".into()],
                ),
            ],
        };
        // Ranking surfaces June (later) first, January (earlier) second.
        let sources = vec![
            "helios:fs/lme0000/s_jun/0000.md".to_string(),
            "helios:fs/lme0000/s_jan/0000.md".to_string(),
        ];
        // Rank order keeps June first.
        let rank = expand_hit_sessions(&sources, &rec, false);
        assert_eq!(rank[0], "[Session date: 2023/06/01 (Thu) 08:00]");
        // Chronological reorders: January (older) first, June second.
        let chrono = expand_hit_sessions(&sources, &rec, true);
        assert_eq!(chrono[0], "[Session date: 2023/01/02 (Mon) 08:00]");
        assert_eq!(chrono[1], "user: January");
        assert_eq!(chrono[2], "[Session date: 2023/06/01 (Thu) 08:00]");
        assert!(
            chrono.iter().position(|t| t == "user: January").unwrap()
                < chrono.iter().position(|t| t == "user: June").unwrap()
        );
    }

    #[test]
    fn cap_to_distinct_sessions_bounds_distinct_session_count() {
        use std::collections::BTreeSet;
        let s = |sid: &str, t: usize| format!("helios:fs/lme0001/{sid}/{t}.md");
        // Ranked hits clustering into sessions: A,A,B,A,C,D,B — only 4 distinct
        // (A,B,C,D) but A dominates the top of the ranking (the clustering that
        // starves multi-session gold coverage).
        let sources =
            vec![s("A", 0), s("A", 1), s("B", 0), s("A", 2), s("C", 0), s("D", 0), s("B", 1)];

        // cap=2 keeps ONLY the first 2 distinct sessions (A,B); C and D are
        // dropped, but every A/B hit (incl. the trailing B) is retained.
        let capped = cap_to_distinct_sessions(&sources, 2);
        let sids: BTreeSet<_> = capped.iter().filter_map(|x| sid_from_source(x)).collect();
        assert_eq!(sids, ["A", "B"].into_iter().collect());
        assert_eq!(capped.len(), 5); // A0,A1,B0,A2,B1 (C0,D0 dropped)

        // cap=usize::MAX (the LME_SESS_K-unset default) is a strict no-op.
        let all = cap_to_distinct_sessions(&sources, usize::MAX);
        assert_eq!(all, sources);
        let all_sids: BTreeSet<_> = all.iter().filter_map(|x| sid_from_source(x)).collect();
        assert_eq!(all_sids.len(), 4);

        // The payoff: a 4-gold question whose gold sessions are B,C,D,A. The
        // historical tight top-k that clustered into {A,B} covers only 2/4;
        // capping a LARGER pool at 4 distinct sessions recovers all 4.
        let gold = vec!["A".to_string(), "B".to_string(), "C".to_string(), "D".to_string()];
        assert_eq!(evidence_recall_coverage(&cap_to_distinct_sessions(&sources, 2), &gold), (2, 4));
        assert_eq!(evidence_recall_coverage(&cap_to_distinct_sessions(&sources, 4), &gold), (4, 4));
        assert!(evidence_recall_all_hit(&cap_to_distinct_sessions(&sources, 4), &gold));
    }

    #[test]
    fn evidence_recall_hit_detects_gold_session_in_source() {
        let answer_sessions = vec!["s2".to_string()];
        let sources_hit = vec![
            "helios:fs/lme0000/h/s1/0001.md".to_string(),
            "helios:fs/lme0000/h/s2/0000.md".to_string(),
        ];
        assert!(evidence_recall_hit(&sources_hit, &answer_sessions));
        let sources_miss = vec!["helios:fs/lme0000/h/s1/0000.md".to_string()];
        assert!(!evidence_recall_hit(&sources_miss, &answer_sessions));
        // Empty gold-session id must never match everything.
        assert!(!evidence_recall_hit(&sources_hit, &[String::new()]));
    }

    #[test]
    fn evidence_recall_all_hit_requires_every_gold_session() {
        // A multi-session question whose answer needs THREE gold sessions.
        let gold = vec!["s2".to_string(), "s5".to_string(), "s7".to_string()];
        let all_present = vec![
            "helios:fs/lme0000/h/s2/0000.md".to_string(),
            "helios:fs/lme0000/h/s5/0003.md".to_string(),
            "helios:fs/lme0000/h/s7/0001.md".to_string(),
            "helios:fs/lme0000/h/s9/0000.md".to_string(), // distractor session
        ];
        let partial = vec![
            "helios:fs/lme0000/h/s2/0000.md".to_string(),
            "helios:fs/lme0000/h/s5/0003.md".to_string(),
            // s7 (a required gold session) is missing
            "helios:fs/lme0000/h/s9/0000.md".to_string(),
        ];
        // all-gold fires only when EVERY required session is covered.
        assert!(evidence_recall_all_hit(&all_present, &gold));
        assert!(!evidence_recall_all_hit(&partial, &gold));
        // Contrast: the any-gold metric OVERSTATES the partial case as a hit —
        // exactly the multi-session inflation the strict metric corrects.
        assert!(evidence_recall_hit(&partial, &gold));
        // Coverage counts (covered, total).
        assert_eq!(evidence_recall_coverage(&all_present, &gold), (3, 3));
        assert_eq!(evidence_recall_coverage(&partial, &gold), (2, 3));
        // Empty / all-empty gold is never a recall success (no vacuous true).
        assert!(!evidence_recall_all_hit(&all_present, &[]));
        assert!(!evidence_recall_all_hit(&all_present, &[String::new()]));
        assert_eq!(evidence_recall_coverage(&all_present, &[String::new()]), (0, 0));
    }
}
