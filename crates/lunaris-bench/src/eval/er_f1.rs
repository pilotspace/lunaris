//! Plan 05-06 EVAL-03 — Entity Resolution F1 harness. B-7 stub; body lands
//! in Task 3.

#![forbid(unsafe_code)]

use crate::eval::EvalRow;

pub(crate) const HARNESS: &str = "er-f1";
pub(crate) const METRIC: &str = "f1";
pub(crate) const THRESHOLD: f64 = 0.80;

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    results.push(EvalRow::skipped(
        HARNESS,
        METRIC,
        THRESHOLD,
        "B-7 stub — body lands in Task 3 of Plan 05-06",
    ));
    Ok(())
}
