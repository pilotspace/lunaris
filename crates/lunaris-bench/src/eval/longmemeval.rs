//! Plan 05-06 EVAL-01 — LongMemEval J-score harness.
//!
//! Body lands in Task 3 of Plan 05-06; Task 1 ships a B-7 stub that emits
//! `status: "SKIPPED"` so `cargo build --bin lunaris-evals` passes after
//! Task 1 alone.

#![forbid(unsafe_code)]

use crate::eval::EvalRow;

/// Threshold per CONTEXT.md D-19: J ≥ 65 (alpha bar). Used by both the B-7
/// stub (Task 1) and the real harness body (Task 3).
pub(crate) const HARNESS: &str = "longmemeval";
pub(crate) const METRIC: &str = "j_score";
pub(crate) const THRESHOLD: f64 = 65.0;

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    results.push(EvalRow::skipped(
        HARNESS,
        METRIC,
        THRESHOLD,
        "B-7 stub — body lands in Task 3 of Plan 05-06",
    ));
    Ok(())
}
