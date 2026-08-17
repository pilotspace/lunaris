//! Red-first tests for the judge-free **any-gold** tally mode of
//! `scripts/bench/lme/tally.py` (GA-2a recall-ratchet gate).
//!
//! The CI recall gate (`.github/workflows/recall-ratchet.yml` via
//! `scripts/bench/lme/anygold_gate.sh`) runs `lunaris-evals` with
//! `LUNARIS_EVAL_LME_JUDGE` unset — the cheap evidence-recall pass. In that
//! mode the binary emits NO `LME_VERDICT` line (that line is judge-path
//! only), so the classic tally sees every question as ERR. The gate instead
//! scores the per-question debug trace `=> evidence_recall_hit = true|false`
//! (emitted under `LUNARIS_EVAL_LME_DEBUG=1`), preferring the
//! `evidence_recall_hit` key of an `LME_VERDICT` line when one exists.
//!
//! These tests drive `tally.py --anygold` (and its `--baseline` ratchet
//! comparison) as a subprocess over fixture logs, exactly the way the gate
//! script does. Exit-code contract:
//!   0 = ok / within tolerance, 1 = ratchet regression,
//!   5 = run not final (coverage gap or ERR), 6 = config-signature mismatch.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tally_py() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // workspace root
    p.join("scripts/bench/lme/tally.py")
}

fn run_tally(args: &[&str]) -> Output {
    Command::new("python3").arg(tally_py()).args(args).output().expect("spawn python3 tally.py")
}

fn write(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).unwrap();
}

const SIG: &str = "dataset=longmemeval_s|hybrid=1|rerank=1|graph=0|offsets=offsets16.tsv:2";

fn write_baseline(path: &Path, hits: u32, total: u32, tolerance: u32, sig: &str) {
    let body = format!(
        r#"{{
  "metric": "lme_s_anygold",
  "hits": {hits},
  "total": {total},
  "tolerance_questions": {tolerance},
  "config_signature": "{sig}"
}}"#
    );
    fs::write(path, body).unwrap();
}

/// No-judge logs carry only the debug trace line; the anygold tally must
/// score it three ways: hit / miss / ERR (no line at all).
#[test]
fn anygold_scores_debug_trace_lines() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "q71.log", "  [DEBUG q0] ...\n    => evidence_recall_hit = true\n");
    write(tmp.path(), "q75.log", "    => evidence_recall_hit = false\n");
    write(tmp.path(), "q81.log", "watchdog killed before retrieval\n");

    let out = run_tally(&[
        "--dir",
        tmp.path().to_str().unwrap(),
        "--expected",
        "3",
        "--anygold",
        "--json",
    ]);
    assert!(out.status.success(), "tally --anygold failed: {out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json output");
    assert_eq!(v["correct"], serde_json::json!([71]), "q71 must be an any-gold hit");
    assert_eq!(v["wrong"], serde_json::json!([75]), "q75 must be an any-gold miss");
    assert_eq!(v["err"], serde_json::json!([81]), "a log with no recall line is ERR");
    assert_eq!(v["final"], serde_json::json!(false), "ERR>0 can never be final");
    assert_eq!(v["metric"], serde_json::json!("anygold"));
}

/// Judge-mode artifacts stay tallyable in anygold mode: the machine-readable
/// `LME_VERDICT` line's `evidence_recall_hit` key wins over any stray debug
/// trace, and the judge's `correct` verdict is ignored entirely (a wrong
/// ANSWER with gold evidence retrieved is still a retrieval hit).
#[test]
fn anygold_prefers_the_verdict_line_and_ignores_judge_correctness() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "q71.log",
        concat!(
            "    => evidence_recall_hit = false\n", // stale earlier trace
            "LME_VERDICT {\"question_id\":\"x\",\"correct\":false,\"evidence_recall_hit\":true}\n",
        ),
    );
    let out = run_tally(&[
        "--dir",
        tmp.path().to_str().unwrap(),
        "--expected",
        "1",
        "--anygold",
        "--json",
    ]);
    assert!(out.status.success(), "tally --anygold failed: {out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json output");
    assert_eq!(v["correct"], serde_json::json!([71]));
    assert_eq!(v["final"], serde_json::json!(true));
}

/// Ratchet pass: hits within `tolerance_questions` of the baseline → exit 0.
#[test]
fn baseline_within_tolerance_passes() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "q71.log", "    => evidence_recall_hit = true\n");
    write(tmp.path(), "q75.log", "    => evidence_recall_hit = false\n");
    let base = tmp.path().join("baseline.json");
    write_baseline(&base, 2, 2, 1, SIG);

    let out = run_tally(&[
        "--dir",
        tmp.path().to_str().unwrap(),
        "--expected",
        "2",
        "--anygold",
        "--baseline",
        base.to_str().unwrap(),
        "--config-signature",
        SIG,
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "1 hit vs baseline 2 with tolerance 1 must pass: {out:?}"
    );
}

/// Ratchet regression: hits below baseline - tolerance → exit 1, loud output.
#[test]
fn baseline_regression_fails_with_exit_1() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "q71.log", "    => evidence_recall_hit = false\n");
    write(tmp.path(), "q75.log", "    => evidence_recall_hit = false\n");
    let base = tmp.path().join("baseline.json");
    write_baseline(&base, 2, 2, 1, SIG);

    let out = run_tally(&[
        "--dir",
        tmp.path().to_str().unwrap(),
        "--expected",
        "2",
        "--anygold",
        "--baseline",
        base.to_str().unwrap(),
        "--config-signature",
        SIG,
    ]);
    assert_eq!(out.status.code(), Some(1), "0 hits vs baseline 2 tol 1 must fail: {out:?}");
    let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("REGRESSION"), "failure output must say REGRESSION, got: {text}");
}

/// A non-final run (coverage gap or ERR) can never be compared to a
/// baseline — that's the partial-arm lesson. Exit 5, not a pass.
#[test]
fn baseline_refuses_non_final_runs_with_exit_5() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "q71.log", "    => evidence_recall_hit = true\n");
    // expected=2 but only one artifact → not final.
    let base = tmp.path().join("baseline.json");
    write_baseline(&base, 2, 2, 1, SIG);

    let out = run_tally(&[
        "--dir",
        tmp.path().to_str().unwrap(),
        "--expected",
        "2",
        "--anygold",
        "--baseline",
        base.to_str().unwrap(),
        "--config-signature",
        SIG,
    ]);
    assert_eq!(out.status.code(), Some(5), "partial run must exit 5: {out:?}");
}

/// A baseline measured under one retrieval config must never gate a run
/// measured under another. Exit 6 on signature mismatch.
#[test]
fn baseline_refuses_config_signature_mismatch_with_exit_6() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "q71.log", "    => evidence_recall_hit = true\n");
    write(tmp.path(), "q75.log", "    => evidence_recall_hit = true\n");
    let base = tmp.path().join("baseline.json");
    write_baseline(&base, 2, 2, 0, SIG);

    let out = run_tally(&[
        "--dir",
        tmp.path().to_str().unwrap(),
        "--expected",
        "2",
        "--anygold",
        "--baseline",
        base.to_str().unwrap(),
        "--config-signature",
        "dataset=longmemeval_s|hybrid=1|rerank=0|graph=0|offsets=offsets16.tsv:2",
    ]);
    assert_eq!(out.status.code(), Some(6), "config mismatch must exit 6: {out:?}");
}

/// `--write-baseline` emits the ratchet file the gate later compares
/// against: hits/total from THIS run, the passed signature, and a default
/// tolerance of 1 question (cross-platform float jitter can flip one
/// borderline rank; anything more is signal).
#[test]
fn write_baseline_round_trips_through_the_comparison() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "q71.log", "    => evidence_recall_hit = true\n");
    write(tmp.path(), "q75.log", "    => evidence_recall_hit = true\n");
    let base = tmp.path().join("baseline.json");

    let out = run_tally(&[
        "--dir",
        tmp.path().to_str().unwrap(),
        "--expected",
        "2",
        "--anygold",
        "--write-baseline",
        base.to_str().unwrap(),
        "--config-signature",
        SIG,
    ]);
    assert_eq!(out.status.code(), Some(0), "write-baseline must succeed: {out:?}");
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&base).unwrap()).expect("baseline json");
    assert_eq!(v["metric"], serde_json::json!("lme_s_anygold"));
    assert_eq!(v["hits"], serde_json::json!(2));
    assert_eq!(v["total"], serde_json::json!(2));
    assert_eq!(v["tolerance_questions"], serde_json::json!(1));
    assert_eq!(v["config_signature"], serde_json::json!(SIG));

    // And the file it wrote must gate a re-run of the same artifacts green.
    let out = run_tally(&[
        "--dir",
        tmp.path().to_str().unwrap(),
        "--expected",
        "2",
        "--anygold",
        "--baseline",
        base.to_str().unwrap(),
        "--config-signature",
        SIG,
    ]);
    assert_eq!(out.status.code(), Some(0), "round-trip must pass: {out:?}");
}

/// A non-final run must not be blessed as a baseline either.
#[test]
fn write_baseline_refuses_non_final_runs() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "q71.log", "no recall line here\n");
    let base = tmp.path().join("baseline.json");

    let out = run_tally(&[
        "--dir",
        tmp.path().to_str().unwrap(),
        "--expected",
        "1",
        "--anygold",
        "--write-baseline",
        base.to_str().unwrap(),
        "--config-signature",
        SIG,
    ]);
    assert_eq!(out.status.code(), Some(5), "ERR run must not become a baseline: {out:?}");
    assert!(!base.exists(), "no baseline file may be written for a non-final run");
}
