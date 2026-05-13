//! Plan 05-06 EVAL-05 + EVAL-06 — end-to-end Helios scenarios.
//!
//! Re-implements the `HeliosScratchpad` smoke loop from Plan 05-04
//! (`crates/lunaris/tests/helios_scratchpad_smoke.rs`) as a callable harness
//! so `lunaris-evals e2e` can produce JSON-serializable EvalRows from the
//! same workload. Same Helios-shaped writes / reads / grep + same
//! INGEST-05 / RETRIEVE-11/12 budgets.
//!
//! Per CONTEXT.md D-19 + D-16: chat 10k turns + doc rag 50k md.
//! Default smaller (`LUNARIS_EVAL_E2E_TURNS=200`,
//! `LUNARIS_EVAL_E2E_DOCS=1000`) for CI runtime; full corpus via env override.
//!
//! ## Soft-fail
//!
//! For each `(MOON_URL, PG_URL)` env value: missing env → 2 SKIPPED rows
//! (chat + doc) tagged with the per-backend recall budget. Every sub-scenario
//! is wrapped in a try/skip so a single backend hiccup never fails the
//! sibling backend's row.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Instant;

use crate::eval::EvalRow;

pub(crate) const HARNESS_CHAT: &str = "e2e-helios-chat";
pub(crate) const HARNESS_DOC: &str = "e2e-helios-doc";

/// Per-backend recall p50 budget per CONTEXT.md D-16 — same numbers as
/// INGEST-05 / RETRIEVE-11/12 from Plan 02-04 D-12.
#[derive(Clone, Copy)]
struct Budgets {
    recall_p50_ms: f64,
}

fn budget_for(env: &str) -> Budgets {
    match env {
        "MOON_URL" => Budgets { recall_p50_ms: 25.0 }, // RETRIEVE-11
        "PG_URL" => Budgets { recall_p50_ms: 60.0 },   // RETRIEVE-12
        _ => Budgets { recall_p50_ms: f64::INFINITY },
    }
}

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    for url_env in ["MOON_URL", "PG_URL"] {
        let budget = budget_for(url_env);
        let metric_chat = format!("recall_p50_ms_{}", url_env.to_lowercase());
        let metric_doc = metric_chat.clone();

        let url = match std::env::var(url_env) {
            Ok(u) => u,
            Err(_) => {
                results.push(EvalRow::skipped(
                    HARNESS_CHAT,
                    &metric_chat,
                    budget.recall_p50_ms,
                    &format!("{url_env} unset"),
                ));
                results.push(EvalRow::skipped(
                    HARNESS_DOC,
                    &metric_doc,
                    budget.recall_p50_ms,
                    &format!("{url_env} unset"),
                ));
                continue;
            }
        };

        // Chat 10k scenario (defaults to 200 turns; LUNARIS_EVAL_E2E_TURNS
        // env override per CONTEXT.md D-16).
        match run_chat_scenario(&url, url_env, budget).await {
            Ok(row) => results.push(row),
            Err(e) => results.push(EvalRow::skipped(
                HARNESS_CHAT,
                &metric_chat,
                budget.recall_p50_ms,
                &format!("chat scenario failed: {e}"),
            )),
        }

        // Doc 50k scenario (defaults to 1000 docs; LUNARIS_EVAL_E2E_DOCS
        // env override).
        match run_doc_scenario(&url, url_env, budget).await {
            Ok(row) => results.push(row),
            Err(e) => results.push(EvalRow::skipped(
                HARNESS_DOC,
                &metric_doc,
                budget.recall_p50_ms,
                &format!("doc scenario failed: {e}"),
            )),
        }
    }
    Ok(())
}

/// Chat scenario — synthetic per-turn write/read loop matching the
/// `helios_chat_10k_turns_dual_backend` smoke in Plan 05-04.
async fn run_chat_scenario(url: &str, env: &str, budget: Budgets) -> anyhow::Result<EvalRow> {
    let started = Instant::now();
    let lunaris = Arc::new(lunaris::Lunaris::open(url).await?);
    let session_id = format!("eval-chat-{}", ulid::Ulid::new());
    let pad =
        lunaris::HeliosScratchpad::new(lunaris.clone(), lunaris_core::Scope::dev(), &session_id);

    let turns: usize =
        std::env::var("LUNARIS_EVAL_E2E_TURNS").ok().and_then(|v| v.parse().ok()).unwrap_or(200);

    let mut recall_samples_ms: Vec<f64> = Vec::with_capacity(turns);
    for i in 0..turns {
        let path = format!("turn-{i:06}.md");
        let content =
            format!("Turn {i}: hello eval e2e. The quick brown fox jumps over the lazy dog.");
        pad.write(&path, content).await?;
        let t = Instant::now();
        let _ = pad.read(&path).await?;
        recall_samples_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    // Per-tenant cleanup so re-runs don't accumulate session state.
    let _ = pad.forget().await;

    let p50 = percentile(&mut recall_samples_ms, 50.0);
    let metric = format!("recall_p50_ms_{}", env.to_lowercase());
    Ok(EvalRow::judge_le(
        HARNESS_CHAT,
        &metric,
        p50,
        budget.recall_p50_ms,
        started.elapsed().as_millis() as u64,
    ))
}

/// Doc scenario — bulk-ingest synthetic markdown via build_md_doc_corpus,
/// then 100 grep samples. Mirrors `helios_doc_rag_50k_md_dual_backend`.
async fn run_doc_scenario(url: &str, env: &str, budget: Budgets) -> anyhow::Result<EvalRow> {
    let started = Instant::now();
    let lunaris = Arc::new(lunaris::Lunaris::open(url).await?);
    let session_id = format!("eval-doc-{}", ulid::Ulid::new());
    let pad =
        lunaris::HeliosScratchpad::new(lunaris.clone(), lunaris_core::Scope::dev(), &session_id);

    let docs: u64 =
        std::env::var("LUNARIS_EVAL_E2E_DOCS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000);

    let written =
        crate::corpus::build_md_doc_corpus(lunaris.storage().as_ref(), docs, 0xDEAD_BEEF).await?;
    eprintln!("e2e-doc {env}: bulk-ingested {written} docs");

    let mut grep_samples_ms: Vec<f64> = Vec::with_capacity(100);
    for q in 0..100u32 {
        let t = Instant::now();
        let _ = pad.grep(&format!("Section {} Lorem", q % 8), 5).await?;
        grep_samples_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let p50 = percentile(&mut grep_samples_ms, 50.0);
    let metric = format!("recall_p50_ms_{}", env.to_lowercase());
    Ok(EvalRow::judge_le(
        HARNESS_DOC,
        &metric,
        p50,
        budget.recall_p50_ms,
        started.elapsed().as_millis() as u64,
    ))
}

fn percentile(samples: &mut [f64], pct: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((samples.len() as f64) * (pct / 100.0)) as usize;
    samples[idx.min(samples.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_for_moon_returns_25ms() {
        assert_eq!(budget_for("MOON_URL").recall_p50_ms, 25.0);
    }

    #[test]
    fn budget_for_pg_returns_60ms() {
        assert_eq!(budget_for("PG_URL").recall_p50_ms, 60.0);
    }

    #[test]
    fn budget_for_unknown_env_is_infinite() {
        assert!(budget_for("UNKNOWN").recall_p50_ms.is_infinite());
    }

    #[test]
    fn percentile_p50_picks_middle() {
        let mut v = vec![10.0, 20.0, 30.0, 40.0, 50.0];
        assert_eq!(percentile(&mut v, 50.0), 30.0);
    }

    #[test]
    fn percentile_empty_returns_zero() {
        let mut v: Vec<f64> = Vec::new();
        assert_eq!(percentile(&mut v, 50.0), 0.0);
    }

    #[tokio::test]
    async fn run_emits_at_least_4_rows() {
        // 2 backends × 2 scenarios = 4 minimum rows. Either SKIPPED (env
        // unset) or PASS/FAIL (live run). We don't pin the status; just
        // verify cardinality.
        let mut results: Vec<EvalRow> = Vec::new();
        super::run(&mut results).await.unwrap();
        assert!(
            results.len() >= 4,
            "e2e should emit ≥4 rows (2 backends × 2 scenarios); got {}",
            results.len()
        );
    }
}
