//! RED suite for `observability-rollout-maturity` — wiring `lunaris_eval_score`.
//!
//! The gap (mem0-gap-analysis §3, `metrics.rs:130`): the `lunaris_eval_score`
//! gauge is registered but never `.set()` outside its own unit test, so
//! `/metrics` always reports 0 — it does NOT reflect the last eval run.
//!
//! Contract (TASK.md §3, FROZEN v1): a boot-time loader parses the
//! `eval-results.json` array and publishes `lunaris_eval_score{harness}=value`;
//! missing / unreadable / malformed input soft-fails (0 rows, no panic).
//!
//! RED reason: `lunaris_server::eval_score` does not exist yet.

use std::path::PathBuf;

/// Each test uses a UNIQUE harness label so the process-global prometheus
/// registry can't leak values between tests.
fn gauge(harness: &str) -> f64 {
    lunaris_server::metrics::metrics().eval_score.with_label_values(&[harness]).get()
}

#[test]
fn eval_score_published_from_results_file() {
    let harness = "obsv-published-longmemeval";
    let json = serde_json::json!([
        { "harness": harness, "metric": "j_score", "value": 0.82, "threshold": 0.7,
          "status": "PASS", "duration_ms": 1234 }
    ])
    .to_string();

    let applied = lunaris_server::eval_score::apply_eval_scores(&json);

    assert_eq!(applied, 1, "exactly one eval row should publish");
    assert!(
        (gauge(harness) - 0.82).abs() < f64::EPSILON,
        "lunaris_eval_score{{harness={harness}}} must reflect the row value 0.82, got {}",
        gauge(harness)
    );
}

#[test]
fn eval_score_missing_file_leaves_gauge_unchanged() {
    let missing = PathBuf::from(format!("/nonexistent/lunaris-eval-{}.json", ulid::Ulid::new()));
    // Must not panic and must report zero rows applied.
    let applied = lunaris_server::eval_score::apply_eval_scores_from_path(&missing);
    assert_eq!(applied, 0, "a missing eval-results file is a soft no-op, not a failure");
}

#[test]
fn eval_score_malformed_file_soft_fails() {
    let applied = lunaris_server::eval_score::apply_eval_scores("{ this is not valid json");
    assert_eq!(applied, 0, "malformed eval JSON must soft-fail to zero rows, never panic");
}
