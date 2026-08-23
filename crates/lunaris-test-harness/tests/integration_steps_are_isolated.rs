//! Every live-Moon step in `integration.yml` must start from a fresh Moon.
//!
//! `integration.yml` runs six `cargo test` steps in sequence against ONE Moon
//! that is never reset. A suite can therefore pass on state a PREVIOUS step
//! wrote, and report success for something it never established itself. That
//! is not hypothetical — F28 found `keyword_bm25` green only because an
//! earlier step had created the `chunks` index it searched; on a fresh Moon it
//! failed with `no such index`.
//!
//! An audit (2026-08-23) ran all six steps twice: once in CI's shape, once
//! with a verified-fresh Moon per step. Both arms were fully green, so no
//! suite depends on a predecessor TODAY. This guard exists so that stays true
//! — nothing else makes a new dependency visible, and the failure mode is a
//! green tick over an untested assertion.
//!
//! ## Why a reset and not a flush
//!
//! `FLUSHALL` drops keys and LEAVES FT INDICES STANDING — measured against
//! Moon 0.8.5: after `FLUSHALL`, `FT.INFO <idx>` still returns the index with
//! its schema and its sticky quantization tier. Index-level pollution is
//! exactly what F28 was, so a flush would not have caught it. The reset must
//! restart the server on a fresh data directory.
//!
//! ## Why `--dir` is part of the contract
//!
//! Moon's `--dir` defaults to auto-resolution: the cwd only when it already
//! holds moon persistence data, otherwise the PLATFORM USER-DATA directory.
//! A restart without an explicit, freshly-emptied `--dir` therefore reopens
//! the same store and resets nothing. This cost the F28 audit two rounds of
//! wrong analysis before a `DBSIZE` assertion caught it.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate>")
        .to_path_buf()
}

fn workflow() -> String {
    let p: PathBuf = repo_root().join(".github/workflows/integration.yml");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// `- name:` titles in file order, paired with their line numbers.
fn steps() -> Vec<(usize, String)> {
    workflow()
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let t = l.trim();
            t.strip_prefix("- name:").map(|n| (i + 1, n.trim().to_string()))
        })
        .collect()
}

/// A step that runs the test suites against the job's shared Moon.
fn is_live_test_step(name: &str) -> bool {
    name.starts_with("Run ")
}

/// A step that restarts Moon on an empty data directory.
fn is_reset_step(name: &str) -> bool {
    name.contains("Reset Moon") || name.contains("Launch Moon")
}

#[test]
fn every_live_test_step_is_preceded_by_a_moon_reset() {
    let steps = steps();
    let mut offenders = Vec::new();
    for (idx, (line, name)) in steps.iter().enumerate() {
        if !is_live_test_step(name) {
            continue;
        }
        let preceded = idx > 0 && is_reset_step(&steps[idx - 1].1);
        if !preceded {
            let prev = if idx > 0 { steps[idx - 1].1.as_str() } else { "<first step>" };
            offenders.push(format!("  line {line}: {name:?}\n    preceded by: {prev:?}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "these live-Moon test steps run against whatever the previous step left behind:\n{}\n\n\
         Each must be immediately preceded by a step that restarts Moon on an EMPTY data \
         directory. A `FLUSHALL` is not enough — it leaves FT indices standing, and an \
         index left over from a previous step is exactly the F28 defect.",
        offenders.join("\n")
    );
}

/// Every place in CI that starts a Moon: the workflow itself plus the scripts
/// it calls. The launch moved into `scripts/ci/reset_moon.sh`, so a guard that
/// read only the workflow would have gone green by finding nothing.
fn launch_sites() -> Vec<String> {
    let mut bodies = vec![workflow()];
    let ci = repo_root().join("scripts/ci");
    if let Ok(entries) = std::fs::read_dir(&ci) {
        for e in entries.flatten() {
            if e.path().extension().is_some_and(|x| x == "sh")
                && let Ok(b) = std::fs::read_to_string(e.path())
            {
                bodies.push(b);
            }
        }
    }
    bodies
        .iter()
        .flat_map(|b| b.lines())
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        // Two spellings: the workflow used to launch inline (`moon --port`);
        // the script launches via `"$BIN" --port "$PORT"`. Matching only the
        // first is how this guard would go quietly green after the move.
        .filter(|l| l.contains("moon --port") || l.contains("--port \"$PORT\""))
        .map(str::to_string)
        .collect()
}

#[test]
fn every_moon_launch_pins_an_explicit_dir() {
    let launches = launch_sites();
    let missing: Vec<&String> = launches.iter().filter(|l| !l.contains("--dir")).collect();
    assert!(
        missing.is_empty(),
        "these Moon launches omit --dir:\n{missing:#?}\n\nWithout it Moon auto-resolves to \
         the platform user-data directory, so a restart reopens the SAME store and the \
         reset is a no-op. The reset is only real if the directory is explicit and emptied."
    );
}

/// Vacuity floor. Both tests above pass on an empty parse — no steps found
/// means no offenders and no launches.
#[test]
fn the_parse_finds_the_steps_it_is_meant_to_check() {
    let steps = steps();
    let live: Vec<_> = steps.iter().filter(|(_, n)| is_live_test_step(n)).collect();
    assert!(
        live.len() >= 6,
        "expected the six `Run ...` live-Moon steps; found {}: {live:#?}",
        live.len()
    );
    let launches = launch_sites();
    assert!(
        !launches.is_empty(),
        "found no Moon launch line in the workflow or scripts/ci/*.sh; the --dir test \
         asserts nothing"
    );
}

/// Contexts GitHub allows in a JOB-level `env:` block. `runner` is NOT among
/// them — it exists only at step level and inside `run:`.
///
/// Getting this wrong invalidates the WHOLE workflow file: every job fails
/// before any step runs, GitHub reports only "this run likely failed because
/// of a workflow file issue", and the failure does not appear in the pull
/// request's `statusCheckRollup` at all. So a board that looks green is not
/// evidence the workflow parsed. Cost one red push to learn.
const JOB_ENV_ALLOWED: &[&str] =
    &["github", "needs", "strategy", "matrix", "vars", "inputs", "env", "secrets"];

#[test]
fn the_job_env_block_uses_only_contexts_github_allows() {
    let body = workflow();
    let mut in_job_env = false;
    let mut offenders = Vec::new();
    for (n, line) in body.lines().enumerate() {
        let indent = line.len() - line.trim_start().len();
        if line.trim() == "env:" && indent == 4 {
            in_job_env = true;
            continue;
        }
        if in_job_env {
            // The block ends at the next key at the same indent.
            if !line.trim().is_empty() && indent <= 4 && !line.trim().starts_with('#') {
                in_job_env = false;
                continue;
            }
            for cap in line.split("${{").skip(1) {
                let expr = cap.split("}}").next().unwrap_or("").trim();
                let root = expr.split(['.', ' ', '(']).next().unwrap_or("").trim();
                if !root.is_empty() && !JOB_ENV_ALLOWED.contains(&root) {
                    offenders.push(format!("  line {}: ${{{{ {expr} }}}}", n + 1));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the job-level `env:` block references contexts GitHub does not allow there:\n{}\n\n\
         Allowed: {JOB_ENV_ALLOWED:?}. Anything else makes the entire workflow file invalid — \
         no job runs, and the failure never reaches the PR's check rollup. Export the value \
         from a `run:` step into $GITHUB_ENV instead.",
        offenders.join("\n")
    );
}
