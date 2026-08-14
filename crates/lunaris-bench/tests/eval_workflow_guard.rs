//! Red-first structural guard: the gauntlet CI workflow must be a RUNNABLE,
//! SKIP-clean gate — not a phantom that can never produce numbers (frozen
//! contract §3, eval-gauntlet-ci-gate).
//!
//! RED until BUILD reworks `.github/workflows/eval-gauntlet.yml` off the
//! unpublished `services: moondb/moon` image (→ manual docker-run, the
//! integration.yml pattern) and onto the weights-cached self-hosted runner
//! (like `llm-gates.yml`), so real numbers populate at HUMAN-UAT.

use std::path::Path;

fn workflow_src() -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/eval-gauntlet.yml");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Collect the lines belonging to the **job-level** `env:` blocks
/// (`jobs.<job_id>.env`) — an `env:` key at exactly 4 spaces of indentation in
/// this file's 2-space-per-level layout. Step-level `env:` (8 spaces) is
/// deliberately excluded: the runner-side contexts ARE legal there.
fn job_level_env_lines(yml: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = yml.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_end() == "    env:" {
            let mut j = i + 1;
            while j < lines.len() {
                let body = lines[j];
                let indent = body.len() - body.trim_start().len();
                // A blank line does not terminate a YAML block; a line at the
                // same-or-shallower indent does.
                if !body.trim().is_empty() && indent <= 4 {
                    break;
                }
                out.push((j + 1, body.to_string()));
                j += 1;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

/// Return the body of one `jobs.<job_id>:` block (the lines indented deeper
/// than the 2-space job key), so an assertion can target a single job instead
/// of the whole file.
fn job_block(yml: &str, job_id: &str) -> String {
    let lines: Vec<&str> = yml.lines().collect();
    let header = format!("  {job_id}:");
    let start = lines
        .iter()
        .position(|l| l.trim_end() == header)
        .unwrap_or_else(|| panic!("job `{job_id}:` not found in eval-gauntlet.yml"));
    let mut body = Vec::new();
    for line in &lines[start + 1..] {
        let indent = line.len() - line.trim_start().len();
        if !line.trim().is_empty() && indent <= 2 {
            break;
        }
        body.push(*line);
    }
    body.join("\n")
}

/// Contexts that do NOT exist inside `jobs.<job_id>.env`. That map is expanded
/// by the Actions **service**, before a runner is assigned, so only
/// `github` / `inputs` / `matrix` / `needs` / `secrets` / `strategy` / `vars`
/// resolve there. Referencing a runner-side one is not a soft warning: GitHub
/// refuses to load the entire workflow file — the run is recorded as
/// `startup_failure` with an empty `jobs` array and the workflow's registered
/// name stays stuck at its file path (`name:` was never read).
const RUNNER_SIDE_CONTEXTS: &[&str] = &["runner.", "env.", "steps.", "job."];

/// Regression pin for the defect that made this workflow un-loadable from the
/// day it was committed (4638a5c) until this fix: `jobs.eval-gauntlet.env`
/// held `LUNARIS_EVAL_CACHE_DIR: ${{ runner.temp }}/lunaris-eval-cache`.
///
/// Forensics (2026-08-15): 200/200 recorded runs = failure, `jobs: []`, and
/// every one fired on `push` even though `on:` has been `workflow_dispatch`-only
/// since b8601cc — the give-away that GitHub could not read the triggers at
/// all. `actionlint` reports it as
/// `context "runner" is not allowed here` at 58:35.
#[test]
fn gauntlet_workflow_job_env_uses_no_runner_side_context() {
    let yml = workflow_src();
    let offenders: Vec<String> = job_level_env_lines(&yml)
        .into_iter()
        .filter(|(_, l)| !l.trim_start().starts_with('#') && l.contains("${{"))
        .filter(|(_, l)| {
            let squashed: String = l.chars().filter(|c| !c.is_whitespace()).collect();
            RUNNER_SIDE_CONTEXTS.iter().any(|c| squashed.contains(&format!("${{{{{c}")))
        })
        .map(|(n, l)| format!("  line {n}: {}", l.trim()))
        .collect();

    assert!(
        offenders.is_empty(),
        "eval-gauntlet.yml references a runner-side context inside a job-level `env:` block.\n\
         GitHub rejects the WHOLE file for this (startup_failure, zero jobs) — it does not \
         merely skip the step.\n\
         Move the value into a step that writes `$GITHUB_ENV` instead.\n\
         Offending lines:\n{}",
        offenders.join("\n")
    );
}

/// The gauntlet's heavy job can only ever run on a weights-cached self-hosted
/// runner, and this repo has had ZERO registered runners since the candle
/// cutover. A workflow whose only job can never be scheduled dies invisibly
/// (queued forever, no annotation). There must be a cheap always-schedulable
/// preflight that reaches a *visible* decision instead.
#[test]
fn gauntlet_workflow_has_a_cheap_preflight_that_skips_visibly() {
    let yml = workflow_src();
    assert!(
        yml.contains("preflight:"),
        "eval-gauntlet.yml needs a `preflight:` job that runs on a stock GitHub \
         runner, so dispatching the workflow always produces a visible result"
    );
    assert!(
        yml.contains("::notice::eval gauntlet skipped:"),
        "the preflight must announce the skip with a `::notice::eval gauntlet skipped: <reason>` \
         annotation — an invisible skip is what let this gate rot for 7 weeks"
    );
    assert!(
        yml.contains("needs: preflight"),
        "the heavy weights-cached job must be gated behind `needs: preflight` so it is \
         explicitly skipped, not left queueing against an empty runner pool"
    );
}

#[test]
fn gauntlet_workflow_has_no_phantom_moon_service() {
    let yml = workflow_src();
    // Reject: unrunnable_service — a `services:` block can't launch an
    // unpublished/locally-built Moon image; the integration.yml manual
    // `docker run` pattern is the runnable shape.
    assert!(
        !yml.contains("services:"),
        "eval-gauntlet.yml still has a services: block — use a manual docker-run Moon step"
    );
}

#[test]
fn gauntlet_workflow_is_dispatch_only_while_runner_pool_is_empty() {
    let yml = workflow_src();
    // 2026-07-17: the repo has ZERO registered self-hosted runners — the
    // `llm-weights-cached` pool's only other consumer (llm-gates.yml) was
    // deleted in the candle cutover (3856bbb), and every push/PR trigger
    // since fails at dispatch in 0s (red on every push = alarm fatigue).
    // Until a weights-cached runner is registered again, the gauntlet must
    // be manual-dispatch only. When the runner returns, restore the
    // push/pull_request triggers AND flip these assertions.
    assert!(yml.contains("workflow_dispatch"), "eval-gauntlet.yml must stay manually dispatchable");
    assert!(
        !yml.contains("\n  push:"),
        "eval-gauntlet.yml must not auto-trigger on push while no \
         llm-weights-cached runner is registered (0s dispatch failure)"
    );
    assert!(
        !yml.contains("\n  pull_request:"),
        "eval-gauntlet.yml must not auto-trigger on pull_request while no \
         llm-weights-cached runner is registered (0s dispatch failure)"
    );
}

#[test]
fn gauntlet_workflow_targets_the_weights_cached_runner() {
    let yml = workflow_src();
    // Must: real J/F1 numbers need cached model weights → the self-hosted
    // weights-cached runner (mirrors llm-gates.yml), SKIP-clean elsewhere.
    assert!(
        yml.contains("llm-weights-cached"),
        "eval-gauntlet.yml must target the [self-hosted, llm-weights-cached] runner"
    );
    // Job-scoped, not file-scoped: the cheap `preflight` job legitimately runs
    // on ubuntu-latest (that is the whole point — it must be schedulable when
    // the self-hosted pool is empty). Only the job that actually produces
    // numbers is forbidden from running weightless.
    let heavy = job_block(&yml, "eval-gauntlet");
    assert!(
        heavy.contains("runs-on: [self-hosted, llm-weights-cached]"),
        "the eval-gauntlet job must run on [self-hosted, llm-weights-cached]"
    );
    assert!(
        !heavy.contains("ubuntu-latest"),
        "the gauntlet can't produce real numbers on ubuntu-latest (no weights)"
    );
}
