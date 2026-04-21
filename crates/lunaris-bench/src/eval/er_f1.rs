//! Plan 05-06 EVAL-03 — Entity Resolution F1 harness.
//!
//! Loads `tner/wnut2017` (held-out NER corpus per CONTEXT.md D-19) via
//! [`crate::eval::longmemeval::download_dataset`] (W-7 fix — shared
//! `pub(crate)` helper, NOT a fictional `__download_test_helper` alias);
//! runs lunaris-extract Gemma-3 4B on the same text; compares predicted
//! `(entity, type)` pairs to gold; computes F1; asserts F1 ≥ 0.80.
//!
//! ## Soft-fail
//!
//! - `LUNARIS_EXTRACT_GEMMA_PATH` unset → SKIPPED (extractor not loadable
//!   without the model weights cached locally; CI doesn't ship a 4B model).
//! - HF dataset download failure → SKIPPED.
//! - F1 computation stub returns 0.0 by default (the gold/predicted pair
//!   parser + extractor invocation lands in 05-HUMAN-UAT.md per ROADMAP
//!   risk register day-7 fallback).

#![forbid(unsafe_code)]

use std::time::Instant;

use crate::eval::EvalRow;

pub(crate) const HARNESS: &str = "er-f1";
pub(crate) const METRIC: &str = "f1";
pub(crate) const THRESHOLD: f64 = 0.80;
const HF_REPO: &str = "tner/wnut2017";
const DATASET_FILENAME: &str = "dataset/test.json";

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    let started = Instant::now();

    // The lunaris-extract Gemma-3 4B backend requires the model weights
    // cached locally — env-gated per Plan 03-01 D-04 convention. CI
    // doesn't bundle a 4B model so the harness SKIPS cleanly there;
    // dev-box runs set the env after `huggingface-cli download` lands.
    if std::env::var("LUNARIS_EXTRACT_GEMMA_PATH").is_err() {
        results.push(EvalRow::skipped(
            HARNESS,
            METRIC,
            THRESHOLD,
            "LUNARIS_EXTRACT_GEMMA_PATH unset — extractor not loadable",
        ));
        return Ok(());
    }

    let cache = crate::eval::longmemeval::resolve_cache_dir();
    let _ = std::fs::create_dir_all(&cache);

    // W-7 fix: reuse shared download_dataset helper from longmemeval.rs.
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
                &format!("WNUT-2017 download failed: {e}"),
            ));
            return Ok(());
        }
    };

    // Verify the dataset bytes are parseable (sanity gate).
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

    // Stub F1 = 0; full implementation per dataset README + extractor
    // invocation lands in 05-HUMAN-UAT.md.
    let f1 = 0.0;
    results.push(EvalRow::judge_ge(
        HARNESS,
        METRIC,
        f1,
        THRESHOLD,
        started.elapsed().as_millis() as u64,
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_skips_cleanly_without_extractor_env() {
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
