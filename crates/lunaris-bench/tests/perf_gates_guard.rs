//! Structural guard: the CPU perf gate must compare like-for-like hardware.
//!
//! Measured 2026-08-21 across four executed runs of `perf-gates.yml`, the
//! `rerank/1x8` bench falls into two tight clusters on GitHub's `ubuntu-24.04`
//! pool — 1.2943–1.3127 s and 1.5160–1.5283 s. Within-cluster spread is 1.4%
//! and 0.8%; **between** clusters it is 16.8%. Runner *image* version does not
//! correlate (a failing run shared `20260810.271.1` with two passing ones), so
//! the split is the underlying CPU.
//!
//! With ONE shared baseline that made the 5% cliff a coin flip. The baseline
//! was blessed on the fast cluster, so a push landing on the slow one failed
//! with a "regression" of +6–16%. Run 32391855064 blamed `e1666ad` — a commit
//! that adds a CLI crate and whose `Cargo.lock` diff has zero deletions, so it
//! cannot have changed one instruction in `lunaris-llamacpp`. A gate that
//! fails on hardware assignment teaches people to ignore it.
//!
//! Keying the baseline by CPU is what makes 5% a meaningful threshold: the
//! same-hardware spread (1.4%) sits well under it.

use std::path::PathBuf;

fn workflow_src() -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    let p = p.join(".github/workflows/perf-gates.yml");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Job-level `env:` (4-space indent in this file's 2-space layout) is expanded
/// by the Actions service BEFORE a runner is assigned. Runner-side context
/// there makes GitHub reject the ENTIRE file as a startup_failure with an empty
/// jobs array — that is STARTUP-01, which kept `eval-gauntlet.yml` at 200/200
/// failed runs. It is also exactly why the CPU fingerprint CANNOT live here.
#[test]
fn perf_gates_job_env_uses_no_runner_side_context() {
    let yml = workflow_src();
    let mut in_job_env = false;
    let mut offenders = Vec::new();
    for (i, line) in yml.lines().enumerate() {
        if line.starts_with("    env:") {
            in_job_env = true;
            continue;
        }
        if in_job_env {
            let indent = line.len() - line.trim_start().len();
            if !line.trim().is_empty() && indent <= 4 {
                in_job_env = false;
                continue;
            }
            if line.trim_start().starts_with('#') {
                continue;
            }
            for ctx in ["runner.", "env.", "steps.", "job."] {
                if line.replace(' ', "").contains(&format!("${{{{{ctx}")) {
                    offenders.push(format!("{}: {}", i + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "runner-side context in perf-gates.yml job-level env: {offenders:?} — GitHub rejects the whole file (STARTUP-01)"
    );
}

/// The baseline name must be built per-runner, not pinned in the job env.
#[test]
fn the_baseline_is_keyed_by_cpu_not_shared_across_the_pool() {
    let yml = workflow_src();

    // A job-level `BASELINE_NAME:` literal is the shared-baseline regression.
    assert!(
        !yml.contains("      BASELINE_NAME:"),
        "perf-gates.yml pins BASELINE_NAME in the job env again. That is one baseline for the whole ubuntu-24.04 pool, whose two CPU clusters differ by 16.8% — far above the 5% cliff, so the gate fails on hardware assignment rather than on code."
    );
    assert!(
        yml.contains("/proc/cpuinfo"),
        "perf-gates.yml no longer fingerprints the runner CPU; the baseline cannot be hardware-specific without it"
    );
    assert!(
        yml.contains("BASELINE_NAME=${BASELINE_PREFIX}"),
        "the CPU fingerprint is no longer folded into BASELINE_NAME"
    );
}

/// Order matters: `$GITHUB_ENV` only reaches LATER steps. If the fingerprint
/// step ever moves after the restore, `env.BASELINE_NAME` expands to empty and
/// the restore silently fetches nothing — a vacuously green gate that looks
/// exactly like a passing one.
/// The fingerprint must not SILENTLY fall back to a shared name.
///
/// The whole point of keying the baseline by CPU is that GitHub's pool mixes
/// hardware classes that differ 16.8% on this workload while the same class
/// varies 1.4%. A fallback like `model="unknown-cpu"` hands every machine that
/// cannot be fingerprinted ONE shared baseline — which is precisely the
/// hardware lottery this step exists to end, reintroduced quietly.
///
/// This is not hypothetical. `/proc/cpuinfo` on **aarch64 Linux has no
/// `model name` field at all** (it reports `CPU implementer` / `CPU part`), so
/// the day anyone moves this job to `ubuntu-*-arm` the awk yields "" and every
/// ARM runner silently shares a baseline. Failing loudly forces that to be a
/// decision instead of a regression nobody sees.
#[test]
fn the_cpu_fingerprint_never_silently_shares_a_baseline() {
    let yml = workflow_src();
    let step =
        yml.split("Fingerprint the runner CPU").nth(1).expect("the fingerprint step must exist");
    // Slice to the step's REAL boundary — the next `- name:`. A fixed line
    // count does not work here: this step carries ~18 lines of comment before
    // its `run:` block, so `take(20)` reads only prose and every assertion
    // below passes against text that contains no shell at all. That mistake
    // was made and caught here; keep the boundary structural.
    let body: String = match step.find("\n      - name:") {
        Some(end) => step[..end].to_string(),
        None => step.to_string(),
    };
    assert!(
        body.contains("run:"),
        "sliced the fingerprint step down to something with no `run:` block — \
         the assertions below would be inspecting comments, not shell"
    );

    assert!(
        !body.contains("unknown-cpu"),
        "the fingerprint step falls back to a literal shared name. Every runner \
         whose CPU cannot be identified would then gate against the SAME \
         baseline, which is the 16.8%-between-classes lottery this step was \
         added to end. Fail the step instead and say what to extend."
    );
    assert!(
        body.contains("exit 1"),
        "the fingerprint step must FAIL when it cannot derive a CPU model. \
         aarch64 Linux /proc/cpuinfo has no `model name` field, so a silent \
         fallback turns an architecture change into an invisible baseline merge."
    );
}

#[test]
fn the_fingerprint_step_runs_before_the_baseline_is_consumed() {
    let yml = workflow_src();
    let set_at = yml
        .lines()
        .position(|l| l.contains("BASELINE_NAME=${BASELINE_PREFIX}"))
        .expect("no step writes BASELINE_NAME into $GITHUB_ENV");
    let first_use = yml
        .lines()
        .position(|l| l.contains("${{ env.BASELINE_NAME }}"))
        .expect("nothing consumes env.BASELINE_NAME");
    assert!(
        set_at < first_use,
        "perf-gates.yml consumes env.BASELINE_NAME at line {} but only sets it at line {} — it expands to empty and the baseline restore fetches nothing",
        first_use + 1,
        set_at + 1
    );
}
