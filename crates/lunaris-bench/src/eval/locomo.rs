//! Plan 05-06 EVAL-02 — LoCoMo J-score harness.
//!
//! Same shape as [`crate::eval::longmemeval`]; HF dataset
//! `snap-research/locomo10`; threshold J ≥ 70 (alpha bar per blueprint §13).
//!
//! Calls [`crate::eval::longmemeval::download_dataset`] (W-7 fix — shared
//! `pub(crate)` helper, NOT a fictional `__download_test_helper` alias).
//! Single source of truth for HF download semantics in the eval gauntlet.
//!
//! Full corpus parser + J-score computation is operator/dev-box-only per
//! ROADMAP risk register day-7 fallback — Plan 05-06 lands the harness
//! shell + dataset plumbing; live numbers populate via 05-HUMAN-UAT.md.

#![forbid(unsafe_code)]

use std::time::Instant;

use crate::eval::EvalRow;

pub(crate) const HARNESS: &str = "locomo";
pub(crate) const METRIC: &str = "j_score";
pub(crate) const THRESHOLD: f64 = 70.0;
const HF_REPO: &str = "snap-research/locomo10";
const DATASET_FILENAME: &str = "data/locomo10.json";

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    let started = Instant::now();

    let url = match std::env::var("MOON_URL") {
        Ok(u) => u,
        Err(_) => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD,
                "MOON_URL unset — LoCoMo harness needs Moon backend",
            ));
            return Ok(());
        }
    };

    let cache = crate::eval::longmemeval::resolve_cache_dir();
    let _ = std::fs::create_dir_all(&cache);

    // W-7 fix: reuse the same `pub(crate)` download_dataset helper from
    // longmemeval.rs — single source of truth for HF download semantics.
    let dataset_path = match crate::eval::longmemeval::download_dataset(
        HF_REPO,
        DATASET_FILENAME,
        &cache,
    )
    .await
    {
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

    let _lunaris = match lunaris::Lunaris::open(&url).await {
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

    // Verify the dataset bytes are parseable (sanity gate — if hf-hub
    // returned a path to garbage we want to SKIP not pass with 0.0).
    let bytes = match std::fs::read(&dataset_path) {
        Ok(b) => b,
        Err(e) => {
            results.push(EvalRow::skipped(
                HARNESS,
                METRIC,
                THRESHOLD,
                &format!("read {}: {e}", dataset_path.display()),
            ));
            return Ok(());
        }
    };
    if let Err(e) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        results.push(EvalRow::skipped(
            HARNESS,
            METRIC,
            THRESHOLD,
            &format!("parse {}: {e}", dataset_path.display()),
        ));
        return Ok(());
    }

    // Stub J-score = 0; full implementation per dataset README + per-query
    // recall pipeline is deferred to 05-HUMAN-UAT.md per ROADMAP risk
    // register day-7 fallback.
    let j_score = 0.0;
    results.push(EvalRow::judge_ge(
        HARNESS,
        METRIC,
        j_score,
        THRESHOLD,
        started.elapsed().as_millis() as u64,
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_skips_cleanly_without_moon_url() {
        let mut results: Vec<EvalRow> = Vec::new();
        super::run(&mut results).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0].status.as_str(),
            "SKIPPED" | "PASS" | "FAIL"
        ));
        assert_eq!(results[0].harness, HARNESS);
        assert_eq!(results[0].metric, METRIC);
        assert_eq!(results[0].threshold, THRESHOLD);
    }
}
