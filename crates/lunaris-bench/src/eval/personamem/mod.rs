//! PersonaMem — persona-tracking MCQ benchmark on the PRODUCTION path.
//!
//! Dataset: HuggingFace `bowen-upenn/PersonaMem` (MIT). Verified schema and
//! its two traps (Python-`repr` `all_options`, exclusive prefix index) are
//! documented in [`dataset`].
//!
//! ## What is measured
//!
//! TencentDB-Agent-Memory publishes **PersonaMem 76% with memory / 48%
//! without**. This harness produces the comparable number: the same lettered
//! multiple-choice questions, answered by a reader that sees ONLY what
//! `Lunaris` recalls from what `Lunaris` ingested. `LUNARIS_EVAL_PM_NOMEM=1`
//! produces the no-memory floor arm (options only, no retrieval) so our own
//! 48%-equivalent is measured rather than assumed. Every published claim must
//! name the split AND the reader model — the reader is doing the answering.
//!
//! ## Production path, not a shortcut
//!
//! Ingest goes through `CodingSessionMemory::write` → `Lunaris::ingest`
//! (chunk + embed + index) and recall through
//! `Lunaris::recall_with_degraded_check()` with the same hybrid
//! (Vector ∧ BM25 → RRF → cross-encoder rerank → top-k) root the LongMemEval
//! harness measures on. The graph pipeline stays OFF (the default).
//!
//! ## Temporal honesty
//!
//! Questions are grouped by `shared_context_id` and sorted by
//! `end_index_in_shared_context`; the context is ingested INCREMENTALLY and
//! append-only via [`dataset::IngestCursor`], and a question is answered the
//! moment its prefix — and nothing after it — is in the store. Recall
//! therefore cannot see the future by construction. The invariant is also
//! CHECKED at runtime: any hit whose message index is `>= end_index` marks the
//! question ERR with a loud log line rather than silently inflating the score
//! (that shape means the store was not flushed between contexts).
//!
//! ## Run unit = ONE SHARED CONTEXT PER PROCESS
//!
//! `LUNARIS_EVAL_PM_OFFSET` / `_LIMIT` window the **context** list (default
//! limit 1), mirroring LongMemEval's offset/limit convention so the same
//! process-isolated orchestration works. A context's questions run
//! sequentially inside that process — they share one incremental ingest, so
//! splitting them across processes would re-ingest the whole prefix per
//! question. `_QOFFSET` / `_QLIMIT` slice questions within the context (the
//! 2-question smoke).

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::time::Instant;

use crate::eval::EvalRow;

pub(crate) mod dataset;
pub(crate) mod reader;

use dataset::{IngestCursor, PmContextGroup, PmMessage, PmQuestion};
use reader::{PmTally, PmVerdict};

pub(crate) const HARNESS: &str = "personamem";
pub(crate) const METRIC: &str = "accuracy";
/// TencentDB-Agent-Memory's published PersonaMem number. PASS = we match or
/// beat it; the row is only meaningful alongside the split + reader model,
/// both of which the harness logs.
pub(crate) const THRESHOLD: f64 = 76.0;
const HF_REPO: &str = "bowen-upenn/PersonaMem";
const DEFAULT_SPLIT: &str = "32k";
const DEFAULT_MODEL: &str = "claude-sonnet-5";

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| default.to_string())
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn env_flag(key: &str, default: bool) -> bool {
    std::env::var(key).map(|v| v == "1").unwrap_or(default)
}

/// Parse a question-id allowlist file: one id per line, `#` comments and
/// blank lines ignored. Returns `None` when the env var is unset/empty
/// (= answer every question).
fn parse_qids(text: &str) -> HashSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Split id (`32k` | `128k` | `1M`). Rejected values would 404 into a SKIP, so
/// the set is closed here and the mistake is loud.
fn split_id() -> anyhow::Result<String> {
    let s = env_str("LUNARIS_EVAL_PM_SPLIT", DEFAULT_SPLIT);
    match s.as_str() {
        "32k" | "128k" | "1M" => Ok(s),
        other => anyhow::bail!("LUNARIS_EVAL_PM_SPLIT must be 32k|128k|1M, got {other:?}"),
    }
}

/// EVAL-PM entry point. Soft-fail discipline per CONTEXT.md D-21: missing
/// `MOON_URL`, a download failure, an unreachable backend or an empty window
/// is `SKIPPED`, never `FAIL`.
pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    let started = Instant::now();
    let skip = |results: &mut Vec<EvalRow>, reason: String| {
        results.push(EvalRow::skipped(HARNESS, METRIC, THRESHOLD, &reason));
    };

    let split = match split_id() {
        Ok(s) => s,
        Err(e) => {
            skip(results, e.to_string());
            return Ok(());
        }
    };
    let url = match std::env::var("MOON_URL") {
        Ok(u) => u,
        Err(_) => {
            skip(results, "MOON_URL unset — PersonaMem harness needs a Moon backend".into());
            return Ok(());
        }
    };

    let cache = super::longmemeval::resolve_cache_dir();
    let _ = std::fs::create_dir_all(&cache);
    let questions_file = format!("questions_{split}.csv");
    let contexts_file = format!("shared_contexts_{split}.jsonl");
    let questions_path =
        match super::longmemeval::download_dataset(HF_REPO, &questions_file, &cache).await {
            Ok(p) => p,
            Err(e) => {
                skip(results, format!("dataset download ({questions_file}) failed: {e}"));
                return Ok(());
            }
        };
    let contexts_path =
        match super::longmemeval::download_dataset(HF_REPO, &contexts_file, &cache).await {
            Ok(p) => p,
            Err(e) => {
                skip(results, format!("dataset download ({contexts_file}) failed: {e}"));
                return Ok(());
            }
        };

    if let Err(e) = lunaris::Lunaris::open(&url).await {
        skip(results, format!("Lunaris::open({url}) failed: {e}"));
        return Ok(());
    }

    let questions = match std::fs::read(&questions_path)
        .map_err(anyhow::Error::from)
        .and_then(|b| dataset::parse_questions_csv(&b))
    {
        Ok(q) => q,
        Err(e) => {
            skip(results, format!("parse {questions_file}: {e}"));
            return Ok(());
        }
    };
    let groups = dataset::group_by_context(questions);

    let offset = env_usize("LUNARIS_EVAL_PM_OFFSET", 0);
    let limit = env_usize("LUNARIS_EVAL_PM_LIMIT", 1);
    let window: Vec<&PmContextGroup> = groups.iter().skip(offset).take(limit).collect();
    if window.is_empty() {
        // An empty window is a misconfiguration (offset past the dataset).
        // Reporting it as a real 0.0 would trip judge_ge into a bogus FAIL.
        skip(
            results,
            format!(
                "empty context window: offset {offset} >= {} contexts (limit {limit})",
                groups.len()
            ),
        );
        return Ok(());
    }

    let wanted: HashSet<String> = window.iter().map(|g| g.shared_context_id.clone()).collect();
    let contexts = match std::fs::read_to_string(&contexts_path) {
        Ok(t) => dataset::parse_contexts_jsonl(&t, &wanted),
        Err(e) => {
            skip(results, format!("read {contexts_file}: {e}"));
            return Ok(());
        }
    };

    let cfg = RunConfig::from_env(split.clone(), groups.len(), offset, limit);
    eprintln!("{}", cfg.banner());

    let tally = match score_contexts(&url, &window, &contexts, &cfg).await {
        Ok(t) => t,
        Err(e) => {
            skip(results, format!("eval pass failed: {e}"));
            return Ok(());
        }
    };
    if tally.scored == 0 {
        skip(results, format!("no question scored ({} chat/transport errors)", tally.errors));
        return Ok(());
    }

    report(&tally, &cfg);
    results.push(EvalRow::judge_ge(
        HARNESS,
        METRIC,
        tally.accuracy(),
        THRESHOLD,
        started.elapsed().as_millis() as u64,
    ));
    Ok(())
}

/// Every measured knob in one place, so the banner and the runner's config
/// fingerprint describe the same run.
struct RunConfig {
    split: String,
    model: String,
    total_contexts: usize,
    offset: usize,
    limit: usize,
    q_offset: usize,
    q_limit: usize,
    topk: usize,
    pool: usize,
    /// Context-window expansion: each hit message is rendered together with
    /// this many neighbouring messages on each side (clamped to the prefix
    /// bound). PersonaMem docs are single messages, so a bare hit often lands
    /// mid-dialogue — an assistant turn without the user turn that prompted
    /// it. `0` restores bare-hit rendering.
    neighbors: usize,
    /// Per-option evidence retrieval: for each candidate response, the store
    /// is queried with the OPTION TEXT and this many top hits are shown to the
    /// reader as "most similar past messages". This is what exposes RECYCLED
    /// candidates — PersonaMem's fresh-idea distractors restate activities or
    /// suggestions already in the history, invisible to a topk-bounded memory
    /// window. `0` disables the pass.
    evidence: usize,
    /// Reader-ceiling mode: bypass retrieval entirely and hand the reader the
    /// FULL allowed prefix (`messages[..end_index]`). Not a memory-system
    /// measurement — it answers "what would this reader score if retrieval
    /// were perfect and unlimited", which bounds what any retrieval
    /// configuration can achieve with this reader.
    full_ctx: bool,
    /// Optional question-id allowlist (`LUNARIS_EVAL_PM_QIDS_FILE`): when
    /// set, ONLY listed questions are answered — the re-run unit for a
    /// second-reader pass over a prior run's failures. Unlisted questions are
    /// skipped entirely (not scored, not ERR).
    qids: Option<HashSet<String>>,
    hybrid: bool,
    rerank: bool,
    no_memory: bool,
    debug: bool,
    artifact_dir: Option<std::path::PathBuf>,
}

impl RunConfig {
    fn from_env(split: String, total_contexts: usize, offset: usize, limit: usize) -> Self {
        Self {
            split,
            model: env_str("LUNARIS_EVAL_PM_MODEL", DEFAULT_MODEL),
            total_contexts,
            offset,
            limit,
            q_offset: env_usize("LUNARIS_EVAL_PM_QOFFSET", 0),
            q_limit: env_usize("LUNARIS_EVAL_PM_QLIMIT", usize::MAX),
            topk: env_usize("LUNARIS_EVAL_PM_TOPK", 10),
            pool: env_usize("LUNARIS_EVAL_PM_POOL", 30),
            neighbors: env_usize("LUNARIS_EVAL_PM_NEIGHBORS", 1),
            evidence: env_usize("LUNARIS_EVAL_PM_EVIDENCE", 3),
            full_ctx: env_flag("LUNARIS_EVAL_PM_FULLCTX", false),
            qids: std::env::var("LUNARIS_EVAL_PM_QIDS_FILE").ok().filter(|s| !s.is_empty()).map(
                |f| match std::fs::read_to_string(&f) {
                    Ok(t) => parse_qids(&t),
                    Err(e) => {
                        // A missing allowlist must never silently widen the
                        // run to every question: fail closed.
                        eprintln!(
                            "  [personamem] QIDS_FILE {f} unreadable ({e}) — answering NO questions"
                        );
                        HashSet::new()
                    }
                },
            ),
            hybrid: env_flag("LUNARIS_EVAL_PM_HYBRID", true),
            rerank: env_flag("LUNARIS_EVAL_PM_RERANK", true),
            no_memory: env_flag("LUNARIS_EVAL_PM_NOMEM", false),
            debug: env_flag("LUNARIS_EVAL_PM_DEBUG", false),
            artifact_dir: std::env::var("LUNARIS_EVAL_PM_ARTIFACT_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from),
        }
    }

    fn banner(&self) -> String {
        format!(
            "  [personamem] split={} model={} contexts[{}..{}) of {} \
             topk={} pool={} neighbors={} evidence={} fullctx={} qids={} hybrid={} rerank={} memory={}",
            self.split,
            self.model,
            self.offset,
            self.offset + self.limit,
            self.total_contexts,
            self.topk,
            self.pool,
            self.neighbors,
            self.evidence,
            self.full_ctx,
            self.qids.as_ref().map_or_else(|| "all".into(), |q| q.len().to_string()),
            self.hybrid,
            self.rerank,
            if self.no_memory { "OFF (no-memory floor arm)" } else { "ON" },
        )
    }
}

/// Ingest each windowed context incrementally and answer its questions in
/// prefix order. Each context starts from a pristine store — `reset_moon`
/// (`FLUSHALL` + `FT.DROPINDEX`) then a fresh `Lunaris::open` — so one
/// persona's memories can never answer another's question.
async fn score_contexts(
    url: &str,
    window: &[&PmContextGroup],
    contexts: &std::collections::HashMap<String, Vec<PmMessage>>,
    cfg: &RunConfig,
) -> anyhow::Result<PmTally> {
    let chat = super::lme_judge::ChatClient::new()?;
    let mut tally = PmTally::default();

    for (ci, group) in window.iter().enumerate() {
        let Some(messages) = contexts.get(&group.shared_context_id) else {
            eprintln!(
                "  [personamem] context {} absent from the JSONL — skipping its {} question(s)",
                group.shared_context_id,
                group.questions.len()
            );
            continue;
        };
        let questions: Vec<&PmQuestion> = group
            .questions
            .iter()
            .skip(cfg.q_offset)
            .take(cfg.q_limit)
            .filter(|q| cfg.qids.as_ref().is_none_or(|ids| ids.contains(&q.question_id)))
            .collect();
        if questions.is_empty() {
            continue;
        }

        super::longmemeval::reset_moon(url).await?;
        let lunaris = std::sync::Arc::new(lunaris::Lunaris::open(url).await?);
        let session = format!("pm{:04}", cfg.offset + ci);
        let pad = lunaris::CodingSessionMemory::new(
            lunaris.clone(),
            lunaris_core::Scope::dev(),
            &session,
        );
        let mut cursor = IngestCursor::new();
        eprintln!(
            "  [personamem] context {}/{} id={} messages={} questions={}",
            ci + 1,
            window.len(),
            &group.shared_context_id[..8.min(group.shared_context_id.len())],
            messages.len(),
            questions.len(),
        );

        for q in questions {
            // Advance the visible prefix to EXACTLY this question's window.
            // Append-only: nothing already written is rewritten, nothing at or
            // past `end_index` is written.
            let docs = cursor.advance(messages, q.end_index);
            let ingested = docs.len();
            for (idx, body) in docs {
                pad.write(&dataset::doc_key(idx), body).await?;
            }
            // Observability for the honesty invariant: the store's visible
            // window MUST equal this question's prefix end after the advance.
            let visible = cursor.position();
            debug_assert_eq!(visible, q.end_index.min(messages.len()));
            if cfg.debug {
                eprintln!(
                    "    [DEBUG {}] visible_prefix={visible} (+{ingested} new docs)",
                    q.question_id
                );
            }
            let verdict = answer_one(&lunaris, &chat, &session, q, messages, cfg, ingested).await;
            eprintln!("{}", reader::verdict_line(&verdict));
            write_artifact(cfg, &verdict);
            tally.record(&verdict);
            eprintln!(
                "  [personamem] running accuracy={:.1}% ({}/{}) err={}",
                tally.accuracy(),
                tally.correct,
                tally.scored,
                tally.errors
            );
        }
    }
    Ok(tally)
}

/// Retrieve → render → read → score one question.
async fn answer_one(
    lunaris: &std::sync::Arc<lunaris::Lunaris>,
    chat: &super::lme_judge::ChatClient,
    session: &str,
    q: &PmQuestion,
    messages: &[PmMessage],
    cfg: &RunConfig,
    ingested: usize,
) -> PmVerdict {
    // Every verdict derives its retrieval fields from one `Retrieval`, so the
    // honesty max and the coverage set can never disagree.
    let mk = |predicted: Option<char>,
              retrieval: &reader::Retrieval,
              memories: usize,
              error: Option<String>| PmVerdict {
        question_id: q.question_id.clone(),
        question_type: q.question_type.clone(),
        shared_context_id: q.shared_context_id.clone(),
        end_index: q.end_index,
        predicted,
        gold: q.gold_letter,
        correct: predicted == Some(q.gold_letter) && error.is_none(),
        hits: retrieval.hits(),
        memories,
        max_hit_index: retrieval.max_index(),
        hit_indices: retrieval.indices().to_vec(),
        error,
    };

    let sources = if cfg.no_memory || cfg.full_ctx {
        Vec::new()
    } else {
        match retrieve(lunaris, &q.user_message, cfg, cfg.topk).await {
            Ok(s) => s,
            Err(e) => {
                return mk(
                    None,
                    &reader::Retrieval::default(),
                    0,
                    Some(format!("recall failed: {e}")),
                );
            }
        }
    };

    // Hit sources → message indices, chronological, deduped.
    let retrieval = reader::Retrieval::new(
        sources.len(),
        sources.iter().filter_map(|s| dataset::index_from_source(s)).collect(),
    );
    if retrieval.unmapped() > 0 {
        // The cursor is the only writer, so every stored document key parses.
        // A miss means the store carried documents this harness did not write.
        eprintln!(
            "  [personamem] {} of {} hits carried no message index for {} — store not isolated?",
            retrieval.unmapped(),
            retrieval.hits(),
            q.question_id
        );
    }

    // RUNTIME TEMPORAL-HONESTY GUARD. Unreachable while the cursor is the only
    // writer and the store was reset for this context; if it fires, the store
    // carried another prefix's documents and the score would be inflated.
    if let Some(leak) = retrieval.indices().iter().find(|i| **i >= q.end_index) {
        let msg = format!(
            "TEMPORAL LEAK: hit message index {leak} >= prefix end {} for {} \
             (store was not isolated for this context)",
            q.end_index, q.question_id
        );
        eprintln!("  [personamem] {msg}");
        return mk(None, &retrieval, 0, Some(msg));
    }

    // Context-window expansion (post-guard, clamped to the prefix bound):
    // PersonaMem docs are single messages, so a bare hit often lands
    // mid-dialogue — an assistant turn without the user turn that prompted it.
    // Rendering each hit with `cfg.neighbors` messages on each side restores
    // the local dialogue. Clamping to `end_index - 1` means expansion can
    // never leak past the prefix the raw hits were guarded against above.
    let indices = if cfg.full_ctx && !cfg.no_memory {
        // Reader-ceiling mode: the whole allowed prefix, in order.
        (0..q.end_index.min(messages.len())).collect()
    } else {
        expand_with_neighbors(retrieval.indices(), cfg.neighbors, q.end_index)
    };
    let memories: Vec<String> =
        indices.iter().filter_map(|i| messages.get(*i)).map(dataset::render_message).collect();

    // Per-option evidence: query the store with each CANDIDATE's text and show
    // the reader its nearest past messages. A recycled candidate (an activity
    // or suggestion already in the history) surfaces its own near-duplicate;
    // a genuinely new one doesn't. Same store, same guard discipline.
    let mut option_evidence: Vec<(char, Vec<String>)> = Vec::new();
    if !cfg.no_memory && !cfg.full_ctx && cfg.evidence > 0 {
        for (letter, text) in &q.options {
            let srcs = match retrieve(lunaris, text, cfg, cfg.evidence).await {
                Ok(s) => s,
                Err(e) => {
                    return mk(None, &retrieval, 0, Some(format!("evidence recall failed: {e}")));
                }
            };
            let mut ev: Vec<usize> =
                srcs.iter().filter_map(|s| dataset::index_from_source(s)).collect();
            ev.sort_unstable();
            ev.dedup();
            if let Some(leak) = ev.iter().find(|i| **i >= q.end_index) {
                let msg = format!(
                    "TEMPORAL LEAK: evidence hit index {leak} >= prefix end {} for {}",
                    q.end_index, q.question_id
                );
                eprintln!("  [personamem] {msg}");
                return mk(None, &retrieval, 0, Some(msg));
            }
            let rendered: Vec<String> = ev
                .iter()
                .filter_map(|i| messages.get(*i))
                .map(|m| truncate_chars(&dataset::render_message(m), 400))
                .collect();
            option_evidence.push((*letter, rendered));
        }
    }

    let prompt =
        reader::render_mcq_prompt(&memories, &q.user_message, &q.options, &option_evidence);
    if cfg.debug {
        eprintln!(
            "    [DEBUG {}] type={} end={} ingested={ingested} hits={} memories={} session={session}",
            q.question_id,
            q.question_type,
            q.end_index,
            retrieval.hits(),
            memories.len()
        );
    }

    let raw = match chat.chat(&cfg.model, reader::MCQ_SYSTEM_PROMPT, &prompt).await {
        Ok(r) => r,
        // Design-for-failure: a transport failure is ERR for this question and
        // never scored wrong; the run continues.
        Err(e) => {
            return mk(None, &retrieval, memories.len(), Some(format!("chat failed: {e}")));
        }
    };
    let valid: Vec<char> = q.options.iter().map(|(l, _)| *l).collect();
    let predicted = reader::parse_letter(&raw, &valid);
    if predicted.is_none() {
        eprintln!(
            "  [personamem] unparseable reply for {} (scored wrong): {:?}",
            q.question_id,
            raw.chars().take(120).collect::<String>()
        );
    }
    mk(predicted, &retrieval, memories.len(), None)
}

/// The production recall root. Default = hybrid (Vector ∧ BM25 → RRF) reranked
/// by the cross-encoder and cut to top-k — the same configuration the
/// LongMemEval harness measures on. `LUNARIS_EVAL_PM_HYBRID=0` falls back to
/// the plain vector root, `_RERANK=0` drops the cross-encoder.
async fn retrieve(
    lunaris: &std::sync::Arc<lunaris::Lunaris>,
    query: &str,
    cfg: &RunConfig,
    topk: usize,
) -> anyhow::Result<Vec<String>> {
    let hits = if cfg.hybrid {
        let fused = lunaris::Vector::new("chunks", cfg.pool)
            .and(lunaris::Keyword::bm25("chunks", cfg.pool))
            .fuse_rrf(60);
        let builder = lunaris.recall_with_degraded_check().await?;
        // Each leg contributes up to `pool` hits, so the cross-encoder must see
        // `2 * pool` fused candidates — the LongMemEval Bucket-A1 lesson (the
        // `.rerank()` sugar silently caps top_in at 30 regardless of pool).
        if cfg.rerank {
            let reranked = lunaris::RerankRetriever::with_top_in(
                Box::new(fused),
                lunaris.reranker(),
                cfg.pool.saturating_mul(2),
            );
            builder
                .with_root_boxed(Box::new(lunaris::TopRetriever::new(Box::new(reranked), topk)))
                .execute(lunaris::Query::text(query))
                .await?
        } else {
            builder
                .with_root_boxed(Box::new(lunaris::TopRetriever::new(Box::new(fused), topk)))
                .execute(lunaris::Query::text(query))
                .await?
        }
    } else {
        let mut builder = lunaris.recall_with_degraded_check().await?;
        if cfg.rerank {
            builder = builder.rerank(lunaris.reranker());
        }
        builder.top(topk).execute(lunaris::Query::text(query)).await?
    };
    Ok(hits.into_iter().map(|h| h.source).collect())
}

/// Truncate to at most `max` characters on a char boundary, marking the cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

/// Expand hit indices with `neighbors` messages on each side, clamped to the
/// question's prefix (`end_index` exclusive), sorted and deduped. `neighbors`
/// of 0 returns the input untouched, so bare-hit rendering stays reachable.
///
/// Pure by design: expansion runs AFTER the runtime temporal-leak guard, so
/// this clamp is what keeps the widened window inside the prefix bound.
fn expand_with_neighbors(indices: &[usize], neighbors: usize, end_index: usize) -> Vec<usize> {
    if neighbors == 0 || end_index == 0 {
        return indices.to_vec();
    }
    let last_allowed = end_index - 1;
    let mut expanded: Vec<usize> = indices
        .iter()
        .flat_map(|&i| i.saturating_sub(neighbors)..=i.saturating_add(neighbors))
        .filter(|&i| i <= last_allowed)
        .collect();
    expanded.sort_unstable();
    expanded.dedup();
    expanded
}

/// Per-question artifact (`<LUNARIS_EVAL_PM_ARTIFACT_DIR>/<question_id>.json`).
/// Best-effort: a write failure is logged, never fatal — the `PM_VERDICT` log
/// line remains the scoring source of truth.
fn write_artifact(cfg: &RunConfig, v: &PmVerdict) {
    let Some(dir) = &cfg.artifact_dir else { return };
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("  [personamem] artifact dir {} unusable: {e}", dir.display());
        return;
    }
    let path = dir.join(format!("{}.json", v.question_id));
    match serde_json::to_vec_pretty(v) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&path, bytes) {
                eprintln!("  [personamem] artifact write {} failed: {e}", path.display());
            }
        }
        Err(e) => eprintln!("  [personamem] artifact serialize failed: {e}"),
    }
}

/// Final summary: overall accuracy plus the per-question_type breakdown the
/// benchmark's 7 skill categories make meaningful.
fn report(tally: &PmTally, cfg: &RunConfig) {
    eprintln!(
        "[personamem] split={} model={} memory={} accuracy={:.1}% ({}/{}) err={} \
         (Tencent published: 76% with memory / 48% without)",
        cfg.split,
        cfg.model,
        if cfg.no_memory { "OFF" } else { "ON" },
        tally.accuracy(),
        tally.correct,
        tally.scored,
        tally.errors,
    );
    for (qtype, correct, n, pct) in tally.breakdown() {
        eprintln!("[personamem]   {qtype:<48} {pct:>5.1}% ({correct}/{n})");
    }
    // Completion sentinel for the runner's resume check: a process that died
    // mid-context never prints this, so a partial log can never be mistaken
    // for a finished one (the LongMemEval H1 lesson, applied at context grain).
    eprintln!("{}", run_done_line(tally, cfg));
}

/// `PM_RUN_DONE <json>` — emitted only after every question in the window has
/// been scored. `scripts/bench/pm/run_pm.sh::done_clean` requires it AND
/// `"errors":0` before it will skip a context on resume.
fn run_done_line(tally: &PmTally, cfg: &RunConfig) -> String {
    let payload = serde_json::json!({
        "split": cfg.split,
        "model": cfg.model,
        "memory": !cfg.no_memory,
        "offset": cfg.offset,
        "limit": cfg.limit,
        "correct": tally.correct,
        "scored": tally.scored,
        "errors": tally.errors,
        "accuracy": (tally.accuracy() * 10.0).round() / 10.0,
    });
    format!("PM_RUN_DONE {payload}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default split must be one this harness actually accepts — a typo
    /// here would 404 every run into a SKIP that reads like a network problem.
    #[test]
    fn default_split_is_accepted_by_the_closed_set() {
        assert!(matches!(DEFAULT_SPLIT, "32k" | "128k" | "1M"));
    }

    /// The runner keys its resume on this line, so its shape is a contract:
    /// prefix + a JSON payload carrying `errors`.
    #[test]
    fn run_done_line_is_the_runners_completion_contract() {
        let cfg = RunConfig {
            split: "32k".into(),
            model: "claude-sonnet-5".into(),
            total_contexts: 37,
            offset: 3,
            limit: 1,
            q_offset: 0,
            q_limit: usize::MAX,
            topk: 10,
            pool: 30,
            neighbors: 1,
            evidence: 3,
            full_ctx: false,
            qids: None,
            hybrid: true,
            rerank: true,
            no_memory: false,
            debug: false,
            artifact_dir: None,
        };
        let mut tally = PmTally::default();
        tally.scored = 4;
        tally.correct = 3;
        let line = run_done_line(&tally, &cfg);
        assert!(line.starts_with("PM_RUN_DONE "));
        let v: serde_json::Value =
            serde_json::from_str(line.strip_prefix("PM_RUN_DONE ").unwrap()).unwrap();
        assert_eq!(v["errors"], serde_json::json!(0));
        assert_eq!(v["scored"], serde_json::json!(4));
        assert_eq!(v["offset"], serde_json::json!(3));
        assert_eq!(v["split"], serde_json::json!("32k"));
    }

    #[test]
    fn neighbor_expansion_widens_dedupes_and_clamps_to_the_prefix() {
        // Two hits one apart: windows overlap and dedupe.
        assert_eq!(expand_with_neighbors(&[3, 4], 1, 100), vec![2, 3, 4, 5]);
        // Hit at 0: the left edge saturates instead of underflowing.
        assert_eq!(expand_with_neighbors(&[0], 2, 100), vec![0, 1, 2]);
        // Hit at the prefix boundary: expansion NEVER crosses end_index.
        assert_eq!(expand_with_neighbors(&[9], 3, 10), vec![6, 7, 8, 9]);
        // neighbors=0 restores bare-hit rendering byte-for-byte.
        assert_eq!(expand_with_neighbors(&[7, 2], 0, 10), vec![7, 2]);
        // Degenerate empty prefix stays empty-safe.
        assert_eq!(expand_with_neighbors(&[], 1, 0), Vec::<usize>::new());
    }

    #[test]
    fn qids_file_parses_ids_skipping_comments_and_blanks() {
        let set = parse_qids("# failures from 32k-memory-v6\nabc-1\n\n  def-2  \n#x\n");
        assert_eq!(set.len(), 2);
        assert!(set.contains("abc-1") && set.contains("def-2"));
    }

    #[test]
    fn threshold_is_the_published_tencent_number() {
        assert_eq!(THRESHOLD, 76.0);
        assert_eq!(METRIC, "accuracy");
        assert_eq!(HARNESS, "personamem");
    }
}
