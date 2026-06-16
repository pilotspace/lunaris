//! Red-first test that the eval-gauntlet GATE bites on a sub-threshold
//! regression (frozen contract §3, eval-gauntlet-ci-gate). `gate_failed` is
//! the pure predicate the `lunaris-evals` binary uses for its exit code; a
//! discriminating test here proves the gate, instead of trusting `main`.
//! RED until BUILD extracts `gate_failed`.

use lunaris_bench::eval::EvalRow;
use lunaris_bench::eval::results::gate_failed;

#[test]
fn gate_bites_on_any_fail_row() {
    let rows = vec![
        EvalRow::pass("longmemeval", "j_score", 70.0, 65.0, 1),
        EvalRow::skipped("er-f1", "f1", 0.80, "no weights"),
        EvalRow::fail("locomo", "j_score", 12.0, 70.0, 1), // seeded sub-threshold regression
    ];
    assert!(gate_failed(&rows), "a FAIL row must trip the gate (exit 1)");
}

#[test]
fn gate_passes_on_pass_and_skipped_only() {
    let rows = vec![
        EvalRow::pass("longmemeval", "j_score", 70.0, 65.0, 1),
        EvalRow::skipped("er-f1", "f1", 0.80, "no weights"),
        EvalRow::skipped("locomo", "j_score", 70.0, "MOON_URL unset"),
    ];
    assert!(!gate_failed(&rows), "PASS + SKIPPED only must NOT trip the gate (exit 0)");
}

#[test]
fn gate_passes_on_empty_manifest() {
    assert!(!gate_failed(&[]), "no rows → no failure");
}
