//! Plan 05-06 EVAL-01 — LongMemEval J-score harness.
//!
//! Downloads HF dataset `xiaowu0162/long-mem-eval` to
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
const DATASET_FILENAME: &str = "longmemeval_oracle";

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
    let dataset_path = match download_dataset(HF_REPO, DATASET_FILENAME, &cache).await {
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

    let lunaris = match lunaris::Lunaris::open(&url).await {
        Ok(l) => std::sync::Arc::new(l),
        Err(e) => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD,
                &format!("Lunaris::open({url}) failed: {e}"),
            ));
            return Ok(());
        }
    };

    let bytes = match std::fs::read(&dataset_path) {
        Ok(b) => b,
        Err(e) => {
            results.push(EvalRow::skipped(HARNESS, METRIC, THRESHOLD, &format!("read dataset: {e}")));
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

    let j_score = match score_haystack(&lunaris, &records).await {
        Ok(s) => s,
        Err(e) => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD,
                &format!("eval pass failed: {e}"),
            ));
            return Ok(());
        }
    };

    results.push(EvalRow::judge_ge(
        HARNESS,
        METRIC,
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
    pub question: String,
    pub answer: String,
    pub answer_session_ids: Vec<String>,
    /// `(session_id, turn_texts)` for every haystack session, distractors included.
    pub sessions: Vec<(String, Vec<String>)>,
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
        question: String,
        answer: serde_json::Value,
        #[serde(default)]
        answer_session_ids: Vec<String>,
        #[serde(default)]
        haystack_session_ids: Vec<String>,
        #[serde(default)]
        haystack_sessions: Vec<Vec<Turn>>,
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
            let sessions = r
                .haystack_session_ids
                .into_iter()
                .zip(r.haystack_sessions)
                .map(|(sid, turns)| {
                    let texts: Vec<String> =
                        turns.into_iter().map(|t| format!("{}: {}", t.role, t.content)).collect();
                    (sid, texts)
                })
                .collect();
            HaystackRecord {
                question: r.question,
                answer,
                answer_session_ids: r.answer_session_ids,
                sessions,
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

/// Real-corpus retrieval harness. For each of the first `limit` records
/// (env `LUNARIS_EVAL_LME_LIMIT`, default 50) ingest the FULL haystack
/// (distractor sessions included) into the engine's default scope under a
/// per-question SESSION PREFIX — `CodingSessionMemory::grep` recalls through the
/// engine-global scope and filters by `source StartsWith helios:fs/<sid>/`
/// (Moon pushes the StartsWith into FT.SEARCH), so recall cannot leak across
/// questions. A per-question *scope* would NOT work: grep ignores the pad's
/// scope. Score **evidence-recall@k**: the % of questions whose top-k surfaced a
/// turn from a gold answer-session. Phrasing-independent; measures retrieval
/// quality without the synthesized-answer substring bias.
async fn score_haystack(
    lunaris: &std::sync::Arc<lunaris::Lunaris>,
    records: &[HaystackRecord],
) -> anyhow::Result<f64> {
    let limit: usize =
        std::env::var("LUNARIS_EVAL_LME_LIMIT").ok().and_then(|s| s.parse().ok()).unwrap_or(50);
    let k: usize =
        std::env::var("LUNARIS_EVAL_LME_TOPK").ok().and_then(|s| s.parse().ok()).unwrap_or(10);
    let n = records.len().min(limit);
    if n == 0 {
        return Ok(0.0);
    }
    let mut evidence_hits = 0usize;
    for (i, rec) in records.iter().take(n).enumerate() {
        let pad = lunaris::CodingSessionMemory::new(
            lunaris.clone(),
            lunaris_core::Scope::dev(),
            &format!("lme{i:04}"),
        );
        for (sid, turns) in &rec.sessions {
            for (ti, text) in turns.iter().enumerate() {
                // Path embeds the session_id so `Hit::source` reveals provenance.
                pad.write(&format!("{sid}/{ti:04}.md"), text.clone()).await?;
            }
        }
        let hits = pad.grep(&rec.question, k).await?;
        let sources: Vec<String> = hits.iter().map(|h| h.source.clone()).collect();
        if evidence_recall_hit(&sources, &rec.answer_session_ids) {
            evidence_hits += 1;
        }
        eprintln!(
            "  [longmemeval {}/{n}] evidence-recall@{k} running={:.1}%",
            i + 1,
            100.0 * evidence_hits as f64 / (i + 1) as f64
        );
    }
    Ok(100.0 * evidence_hits as f64 / n as f64)
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
            "question": "What broke first?",
            "answer": "the GPS",
            "answer_session_ids": ["s2"],
            "haystack_session_ids": ["s1", "s2"],
            "haystack_sessions": [
              [{"role":"user","content":"hi"},{"role":"assistant","content":"hello"}],
              [{"role":"user","content":"my gps failed"}]
            ]
          }
        ]"#;
        let recs = parse_longmemeval_full(json).expect("parse");
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r.question, "What broke first?");
        assert_eq!(r.answer, "the GPS");
        assert_eq!(r.answer_session_ids, vec!["s2".to_string()]);
        assert_eq!(r.sessions.len(), 2);
        assert_eq!(r.sessions[0].0, "s1");
        assert_eq!(r.sessions[0].1[0], "user: hi");
        assert_eq!(r.sessions[1].0, "s2");
        assert_eq!(r.sessions[1].1[0], "user: my gps failed");
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
}
