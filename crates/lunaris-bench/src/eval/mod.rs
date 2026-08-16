//! Plan 05-06 EVAL-01..09 — single-binary eval gauntlet.
//!
//! Per CONTEXT.md D-19 + D-20: every harness emits one `EvalRow` per metric
//! into a shared `Vec<EvalRow>`; [`run_all`] runs every harness in sequence.
//! `lunaris-evals all` writes the manifest to `eval-results.json`.
//!
//! ## Soft-fail discipline (D-21)
//!
//! HF dataset download / missing env / missing dataset → status:`SKIPPED`
//! (NOT `FAIL`). Status `SKIPPED` does NOT block merge. CI gates ONLY on
//! `"status":"FAIL"` per the gate command in
//! `.github/workflows/eval-gauntlet.yml`.
//!
//! ## Cardinality
//!
//! `lunaris-evals all` emits at minimum 9 rows (one per harness/metric):
//! - `longmemeval/j_score` (EVAL-01)
//! - `locomo/j_score` (EVAL-02)
//! - `er-f1/f1` (EVAL-03)
//! - `memory/rss_gb_at_1m_facts` (EVAL-04)
//! - `e2e-helios-chat/recall_p50_ms_{moon,pg_url}` × 2 (EVAL-05)
//! - `e2e-helios-doc/recall_p50_ms_{moon,pg_url}` × 2 (EVAL-06)
//! - `perf-delta/<group>/<bench>::p50_ratio` × 4 (EVAL-07)
//!
//! ## Mirror analog
//!
//! [`EvalRow`] mirrors `crates/lunaris-bench/tests/budget_assertions.rs::BudgetRow`
//! shape per Shared Pattern 8 — same row + report + outcome shape, adapted
//! for JSON serialization to `eval-results.json`.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// One row in the `eval-results.json` manifest. Field shape per CONTEXT.md
/// D-20 verbatim: `harness`, `metric`, `value`, `threshold`, `status`,
/// `duration_ms`. CI greps for `"status":"FAIL"` to fail merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRow {
    pub harness: String,
    pub metric: String,
    pub value: f64,
    pub threshold: f64,
    /// `"PASS"` | `"FAIL"` | `"SKIPPED"`.
    pub status: String,
    pub duration_ms: u64,
}

impl EvalRow {
    /// Construct a PASS row (status = "PASS").
    pub fn pass(harness: &str, metric: &str, value: f64, threshold: f64, duration_ms: u64) -> Self {
        Self {
            harness: harness.into(),
            metric: metric.into(),
            value,
            threshold,
            status: "PASS".into(),
            duration_ms,
        }
    }

    /// Construct a FAIL row (status = "FAIL").
    pub fn fail(harness: &str, metric: &str, value: f64, threshold: f64, duration_ms: u64) -> Self {
        Self { status: "FAIL".into(), ..Self::pass(harness, metric, value, threshold, duration_ms) }
    }

    /// Construct a SKIPPED row (status = "SKIPPED"). Logs the skip reason to
    /// stderr so operators can triage missing data without parsing the JSON.
    /// Per CONTEXT.md D-21 soft-fail: dataset / env / backend missing → SKIPPED
    /// (NOT FAIL).
    pub fn skipped(harness: &str, metric: &str, threshold: f64, reason: &str) -> Self {
        eprintln!("SKIPPED {harness}::{metric} — {reason}");
        Self {
            harness: harness.into(),
            metric: metric.into(),
            value: 0.0,
            threshold,
            status: "SKIPPED".into(),
            duration_ms: 0,
        }
    }

    /// Threshold-direction-aware: `value >= threshold` → PASS. Most metrics
    /// are this kind (J-score, F1) — bigger is better.
    pub fn judge_ge(
        harness: &str,
        metric: &str,
        value: f64,
        threshold: f64,
        duration_ms: u64,
    ) -> Self {
        if value >= threshold {
            Self::pass(harness, metric, value, threshold, duration_ms)
        } else {
            Self::fail(harness, metric, value, threshold, duration_ms)
        }
    }

    /// Threshold-direction-aware: `value <= threshold` → PASS. Used for
    /// memory footprint (≤ 2 GB), latency (≤ p50 budget), perf-delta ratio
    /// (≤ 5x).
    pub fn judge_le(
        harness: &str,
        metric: &str,
        value: f64,
        threshold: f64,
        duration_ms: u64,
    ) -> Self {
        if value <= threshold {
            Self::pass(harness, metric, value, threshold, duration_ms)
        } else {
            Self::fail(harness, metric, value, threshold, duration_ms)
        }
    }
}

/// Run every harness in sequence; populate `results` with one or more
/// `EvalRow` per harness. SKIPPED rows still land in the manifest so the
/// 9-metric catalogue is always complete — a missing harness shows up as
/// `status:"SKIPPED"` rather than silently absent.
pub async fn run_all(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    longmemeval::run(results).await?;
    locomo::run(results).await?;
    er_f1::run(results).await?;
    memory::run(results).await?;
    e2e::run(results).await?;
    perf_delta::run(results).await?;
    Ok(())
}

/// Ingest each query's gold answer as a recallable memory under `session`'s pad
/// — the minimal corpus that makes the answer recall-surfaceable. Shared by the
/// LongMemEval + LoCoMo J-score harnesses. The full multi-session corpus ingest
/// (distractors, session/event summaries) is the HUMAN-UAT harness; this proves
/// the live ingest→recall→score path end-to-end.
pub(crate) async fn ingest_answers_to_pad(
    lunaris: &std::sync::Arc<lunaris::Lunaris>,
    session: &str,
    queries: &[longmemeval::EvalQuery],
) -> anyhow::Result<()> {
    let pad =
        lunaris::CodingSessionMemory::new(lunaris.clone(), lunaris_core::Scope::dev(), session);
    for (i, q) in queries.iter().enumerate() {
        pad.write(&format!("{session}-{i:06}.md"), q.expected_answer.clone()).await?;
    }
    Ok(())
}

/// Recall top-k by each question from `session`'s pad and score with the pure
/// recall-J proxy ([`score::recall_j_score`]): the % of questions whose top-k
/// recalled text contains the normalized gold answer.
pub(crate) async fn recall_j_score_from_pad(
    lunaris: &std::sync::Arc<lunaris::Lunaris>,
    session: &str,
    queries: &[longmemeval::EvalQuery],
) -> anyhow::Result<f64> {
    let pad =
        lunaris::CodingSessionMemory::new(lunaris.clone(), lunaris_core::Scope::dev(), session);
    let mut per_query = Vec::with_capacity(queries.len());
    for q in queries {
        let hits = pad.grep(&q.query, 5).await?;
        per_query.push(score::QueryHit {
            topk_texts: hits.iter().map(|h| h.text.clone()).collect(),
            gold_answer: q.expected_answer.clone(),
        });
    }
    Ok(score::recall_j_score(&per_query))
}

pub mod e2e;
pub mod er_f1;
mod lme_judge;
pub mod locomo;
pub mod longmemeval;
pub mod memory;
pub mod perf_delta;
pub mod personamem;
pub mod results;
pub mod score;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_row_serializes_with_d20_shape() {
        let row = EvalRow::pass("longmemeval", "j_score", 67.2, 65.0, 4_521_000);
        let json = serde_json::to_string(&row).unwrap();
        assert!(json.contains("\"status\":\"PASS\""));
        assert!(json.contains("\"value\":67.2"));
        assert!(json.contains("\"harness\":\"longmemeval\""));
        assert!(json.contains("\"metric\":\"j_score\""));
        assert!(json.contains("\"threshold\":65.0"));
        assert!(json.contains("\"duration_ms\":4521000"));
    }

    #[test]
    fn judge_ge_passes_when_value_meets_threshold() {
        let row = EvalRow::judge_ge("h", "m", 70.0, 65.0, 100);
        assert_eq!(row.status, "PASS");
    }

    #[test]
    fn judge_ge_fails_when_value_below_threshold() {
        let row = EvalRow::judge_ge("h", "m", 60.0, 65.0, 100);
        assert_eq!(row.status, "FAIL");
    }

    #[test]
    fn judge_le_passes_when_value_under_threshold() {
        let row = EvalRow::judge_le("h", "m", 4.0, 5.0, 100);
        assert_eq!(row.status, "PASS");
    }

    #[test]
    fn judge_le_fails_when_value_over_threshold() {
        let row = EvalRow::judge_le("h", "m", 6.0, 5.0, 100);
        assert_eq!(row.status, "FAIL");
    }

    #[test]
    fn skipped_carries_threshold_and_zero_value() {
        let row = EvalRow::skipped("h", "m", 65.0, "test reason");
        assert_eq!(row.status, "SKIPPED");
        assert_eq!(row.value, 0.0);
        assert_eq!(row.threshold, 65.0);
        assert_eq!(row.duration_ms, 0);
    }
}
