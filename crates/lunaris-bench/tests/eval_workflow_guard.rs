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
    assert!(
        !yml.contains("runs-on: ubuntu-latest"),
        "the gauntlet can't produce real numbers on ubuntu-latest (no weights)"
    );
}
