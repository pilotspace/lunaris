//! Plan 05-06 EVAL-04 — RSS sampling at 1M facts. B-7 stub; body lands in
//! Task 2.

#![forbid(unsafe_code)]

use crate::eval::EvalRow;

pub(crate) const HARNESS: &str = "memory";
pub(crate) const METRIC: &str = "rss_gb_at_1m_facts";
pub(crate) const THRESHOLD_GB: f64 = 2.0;

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    results.push(EvalRow::skipped(
        HARNESS,
        METRIC,
        THRESHOLD_GB,
        "B-7 stub — body lands in Task 2 of Plan 05-06",
    ));
    Ok(())
}
