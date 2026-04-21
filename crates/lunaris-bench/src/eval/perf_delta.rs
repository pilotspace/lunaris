//! Plan 05-06 EVAL-07 / EVAL-09 — Moon vs Postgres perf-delta harness. B-7
//! stub; body lands in Task 2.

#![forbid(unsafe_code)]

use crate::eval::EvalRow;

pub(crate) const HARNESS: &str = "perf-delta";
pub(crate) const METRIC: &str = "p50_ratio_postgres_over_moon";
pub(crate) const THRESHOLD: f64 = 5.0;

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    results.push(EvalRow::skipped(
        HARNESS,
        METRIC,
        THRESHOLD,
        "B-7 stub — body lands in Task 2 of Plan 05-06",
    ));
    Ok(())
}
