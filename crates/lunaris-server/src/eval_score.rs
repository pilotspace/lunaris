//! observability-rollout-maturity — publish `lunaris_eval_score` from the last
//! eval run.
//!
//! The eval gauntlet (`lunaris-bench`) writes a JSON **array** of rows to
//! `eval-results.json` (`EvalRow { harness, metric, value, threshold, status,
//! duration_ms }`). This module reads that file at server boot — path from
//! `LUNARIS_EVAL_RESULTS_PATH` — and sets the `lunaris_eval_score{harness}`
//! gauge (`metrics.rs:130`) to each row's `value`, so `/metrics` reflects the
//! last run instead of a constant 0.
//!
//! Missing / unreadable / malformed input SOFT-FAILS (0 rows applied, a single
//! WARN, never a panic, never a failed boot) — mirroring the `EvalRow::skipped`
//! D-21 soft-fail norm. Boot-time only: a long-running server needs a
//! restart/redeploy to reflect a newer eval run (documented residual).

#![forbid(unsafe_code)]

use std::path::Path;

use serde::Deserialize;

use crate::metrics::metrics;

/// Minimal projection of `lunaris_bench::eval::EvalRow` — only the two fields
/// this loader needs. Unknown fields (`metric`, `threshold`, `status`,
/// `duration_ms`) are ignored, so the full manifest deserializes fine without a
/// dependency on `lunaris-bench`.
#[derive(Debug, Deserialize)]
struct EvalScoreRow {
    harness: String,
    value: f64,
}

/// Parse an `eval-results.json` array body and publish each row's score to the
/// `lunaris_eval_score{harness}` gauge. Returns the number of rows applied.
/// Malformed JSON soft-fails to `0` (WARN, no panic).
#[must_use]
pub fn apply_eval_scores(json: &str) -> usize {
    let rows: Vec<EvalScoreRow> = match serde_json::from_str(json) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "lunaris_eval_score: ignoring malformed eval-results JSON");
            return 0;
        }
    };
    let gauge = &metrics().eval_score;
    for row in &rows {
        gauge.with_label_values(&[row.harness.as_str()]).set(row.value);
    }
    rows.len()
}

/// Read an `eval-results.json` file and apply it via [`apply_eval_scores`].
/// A missing / unreadable file soft-fails to `0` (WARN, no panic) — the gauge
/// keeps its current value.
#[must_use]
pub fn apply_eval_scores_from_path(path: &Path) -> usize {
    match std::fs::read_to_string(path) {
        Ok(body) => apply_eval_scores(&body),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "lunaris_eval_score: eval-results file not readable; gauge unchanged"
            );
            0
        }
    }
}

/// Boot hook: if `LUNARIS_EVAL_RESULTS_PATH` is set (and non-empty), publish the
/// scores it names. Unset → no-op (the gauge stays at 0, today's behavior).
/// Returns the number of rows applied (0 when unset). Read-only env access — no
/// `set_var`, so this is safe under `#![forbid(unsafe_code)]`.
pub fn load_eval_scores_from_env() -> usize {
    match std::env::var("LUNARIS_EVAL_RESULTS_PATH") {
        Ok(path) if !path.trim().is_empty() => {
            let n = apply_eval_scores_from_path(Path::new(&path));
            if n > 0 {
                tracing::info!(path = %path, rows = n, "lunaris_eval_score: published last eval run");
            }
            n
        }
        _ => 0,
    }
}
