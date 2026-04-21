//! Plan 05-06 EVAL-05 + EVAL-06 — end-to-end Helios scenarios. B-7 stub;
//! body lands in Task 2.

#![forbid(unsafe_code)]

use crate::eval::EvalRow;

pub(crate) const HARNESS_CHAT: &str = "e2e-helios-chat";
pub(crate) const HARNESS_DOC: &str = "e2e-helios-doc";
pub(crate) const METRIC: &str = "recall_p50_ms";
pub(crate) const THRESHOLD_MS: f64 = 25.0;

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    results.push(EvalRow::skipped(
        HARNESS_CHAT,
        METRIC,
        THRESHOLD_MS,
        "B-7 stub — body lands in Task 2 of Plan 05-06",
    ));
    results.push(EvalRow::skipped(
        HARNESS_DOC,
        METRIC,
        THRESHOLD_MS,
        "B-7 stub — body lands in Task 2 of Plan 05-06",
    ));
    Ok(())
}
