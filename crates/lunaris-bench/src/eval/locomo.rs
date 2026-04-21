//! Plan 05-06 EVAL-02 — LoCoMo J-score harness. B-7 stub; body lands in
//! Task 3.

#![forbid(unsafe_code)]

use crate::eval::EvalRow;

pub(crate) const HARNESS: &str = "locomo";
pub(crate) const METRIC: &str = "j_score";
pub(crate) const THRESHOLD: f64 = 70.0;

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    results.push(EvalRow::skipped(
        HARNESS,
        METRIC,
        THRESHOLD,
        "B-7 stub — body lands in Task 3 of Plan 05-06",
    ));
    Ok(())
}
