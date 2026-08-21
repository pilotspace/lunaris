//! Structural guard: the recall-quality CI gate must be a RUNNABLE,
//! hosted-runner workflow — not a phantom that can never produce numbers.
//!
//! History this file carries forward (GA-2a): the previous gate,
//! `.github/workflows/eval-gauntlet.yml`, never executed once. From its
//! first commit (4638a5c) to 2026-08-15 every recorded run (200/200) was a
//! `startup_failure` with an empty `jobs` array — a `${{ runner.temp }}`
//! reference in a job-level `env:` map makes GitHub reject the ENTIRE file
//! (STARTUP-01) — and after going dispatch-only it targeted a
//! `[self-hosted, llm-weights-cached]` pool with zero registered runners.
//! GA-2a replaced it with `.github/workflows/recall-ratchet.yml`: a
//! judge-free LongMemEval-S any-gold ratchet that runs on `ubuntu-latest`
//! (CPU llamacpp + public HF dataset + a scratch Moon built from
//! vendor/moon), compared against a checked-in measured baseline.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/crates/lunaris-bench.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn workflow_path() -> PathBuf {
    workspace_root().join(".github/workflows/recall-ratchet.yml")
}

fn workflow_src() -> String {
    let p = workflow_path();
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

/// Contexts that do NOT exist inside `jobs.<job_id>.env`. That map is expanded
/// by the Actions **service**, before a runner is assigned, so only
/// `github` / `inputs` / `matrix` / `needs` / `secrets` / `strategy` / `vars`
/// resolve there. Referencing a runner-side one is not a soft warning: GitHub
/// refuses to load the entire workflow file — the run is recorded as
/// `startup_failure` with an empty `jobs` array and the workflow's registered
/// name stays stuck at its file path (`name:` was never read).
const RUNNER_SIDE_CONTEXTS: &[&str] = &["runner.", "env.", "steps.", "job."];

/// STARTUP-01, carried forward from the gauntlet post-mortem: this is the
/// exact defect class that made the previous gate un-loadable for 7 weeks.
#[test]
fn ratchet_workflow_job_env_uses_no_runner_side_context() {
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
        "recall-ratchet.yml references a runner-side context inside a job-level `env:` block.\n\
         GitHub rejects the WHOLE file for this (startup_failure, zero jobs) — it does not \
         merely skip the step.\n\
         Move the value into a step that writes `$GITHUB_ENV` instead.\n\
         Offending lines:\n{}",
        offenders.join("\n")
    );
}

/// The whole point of GA-2a: the recall gate must be schedulable on a stock
/// hosted runner. A `self-hosted` label on any EFFECTIVE line (comments are
/// allowed to tell the gauntlet post-mortem) recreates the phantom-gate
/// failure mode (queued forever against an empty pool).
#[test]
fn ratchet_workflow_runs_on_a_hosted_runner() {
    let yml = workflow_src();
    assert!(
        yml.contains("runs-on: ubuntu-latest"),
        "recall-ratchet.yml must run on ubuntu-latest — a hosted runner that exists"
    );
    let offenders: Vec<String> = yml
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with('#') && l.contains("self-hosted"))
        .map(|(n, l)| format!("  line {}: {}", n + 1, l.trim()))
        .collect();
    assert!(
        offenders.is_empty(),
        "recall-ratchet.yml must never target a self-hosted pool; the eval gauntlet \
         died queued against an empty one. Offending lines:\n{}",
        offenders.join("\n")
    );
}

/// The gate must actually fire: push to main (path-filtered to the
/// recall-affecting surface), a weekly schedule, and manual dispatch.
#[test]
fn ratchet_workflow_has_push_schedule_and_dispatch_triggers() {
    let yml = workflow_src();
    assert!(yml.contains("\n  push:"), "recall-ratchet.yml must trigger on push to main");
    assert!(yml.contains("\n  schedule:"), "recall-ratchet.yml must keep its weekly schedule");
    assert!(
        yml.contains("workflow_dispatch"),
        "recall-ratchet.yml must stay manually dispatchable"
    );
    for path in [
        "crates/lunaris-retrieve/**",
        "crates/lunaris-storage-moon/**",
        "crates/lunaris-ingest/**",
        "crates/lunaris-llamacpp/**",
        "scripts/bench/lme/**",
        ".github/workflows/recall-ratchet.yml",
    ] {
        assert!(
            yml.contains(&format!("\"{path}\"")),
            "recall-ratchet.yml push path filter lost {path} — a recall-affecting \
             change there would silently skip the gate"
        );
    }
}

/// The workflow must run THE gate script against THE checked-in baseline —
/// not re-derive its own scoring inline (copy-paste drift is how the
/// gauntlet's config diverged from the published numbers).
#[test]
fn ratchet_workflow_runs_the_gate_against_the_checked_in_baseline() {
    let yml = workflow_src();
    assert!(
        yml.contains("scripts/bench/lme/anygold_gate.sh"),
        "recall-ratchet.yml must invoke scripts/bench/lme/anygold_gate.sh"
    );
    assert!(
        yml.contains(BASELINE_PATH_TEMPLATE),
        "recall-ratchet.yml must build its baseline path from {BASELINE_PATH_TEMPLATE}"
    );
}

/// The baselines CI gates on after the two-operating-point split
/// (`scripts/bench/lme/baselines/README.md`). Hardcoded, never globbed: a
/// glob over the directory passes vacuously the moment a file is deleted or
/// renamed, which is precisely the failure this guard exists to catch.
const GATING_BASELINES: &[&str] = &["ci-anygold-fast-n40.json", "ci-anygold-quality-n40.json"];

/// The legacy N=16 `quality` baseline. Still coherence-checked while it
/// exists, but deliberately exempt from the sensitivity bar below: its
/// `(1 + 1) / 16` = 12.5-point fail floor IS the defect the N=40 pair
/// replaces (baselines/README.md defect (b1)). Retired in a follow-up once
/// the pair has run green on `main` once.
/// recall-ratchet.yml builds its baseline path from `matrix.point`, so the
/// literal filenames never appear in the workflow. The check that matters is
/// therefore: substitute every DECLARED point into this template and compare
/// the result to GATING_BASELINES.
const BASELINE_PATH_TEMPLATE: &str =
    "scripts/bench/lme/baselines/ci-anygold-${{ matrix.point }}-n40.json";

const LEGACY_BASELINE: &str = "ci-anygold.json";

/// The largest regression, in percentage points, that a gating baseline is
/// allowed to be blind to. `tally.py` fails when `hits < baseline − tolerance`,
/// so the smallest detectable drop is `(tolerance + 1) / total`. Requiring
/// that to be <= 5% is exactly `total >= 20 * (tolerance + 1)` — the general
/// form stated in scripts/bench/lme/baselines/README.md, checked in integer
/// arithmetic so no float rounding sits between the rule and the assertion.
const MAX_BLIND_SPOT_POINTS: u64 = 5;
const MIN_N_PER_TOLERANCE_STEP: u64 = 100 / MAX_BLIND_SPOT_POINTS;

struct Baseline {
    total: u64,
    tolerance: u64,
    signature: String,
}

/// Load a baseline and assert it is internally coherent — including that its
/// config signature's offsets manifest matches the manifest actually in the
/// tree. A silently edited manifest invalidates the ratchet without changing
/// a single number in the baseline file.
fn load_and_check_baseline(name: &str) -> Baseline {
    let p = workspace_root().join("scripts/bench/lme/baselines").join(name);
    let body = std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "read {}: {e}\n  A baseline is a MEASUREMENT — bless it with \
             `anygold_gate.sh --write-baseline` on a machine that actually ran \
             the questions. See scripts/bench/lme/baselines/README.md.",
            p.display()
        )
    });
    let v: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("parse {}: {e}", p.display()));

    assert_eq!(v["metric"], "lme_s_anygold", "{name}: metric must be lme_s_anygold");
    let hits = v["hits"].as_u64().unwrap_or_else(|| panic!("{name}: hits must be an integer"));
    let total = v["total"].as_u64().unwrap_or_else(|| panic!("{name}: total must be an integer"));
    let tolerance = v["tolerance_questions"]
        .as_u64()
        .unwrap_or_else(|| panic!("{name}: tolerance_questions must be an integer"));
    assert!(hits <= total, "{name}: hits {hits} cannot exceed total {total}");
    assert!(total > 1, "{name}: a 1-question baseline ratchets nothing");
    assert!(
        tolerance < total,
        "{name}: tolerance {tolerance} >= total {total} makes the gate vacuous — \
         it could never fail"
    );

    let signature =
        v["config_signature"].as_str().expect("config_signature must be a string").to_string();
    // `offsets=<file>:<count>` must agree with the manifest in the tree.
    let offsets_part = signature
        .split('|')
        .find_map(|part| part.strip_prefix("offsets="))
        .unwrap_or_else(|| panic!("{name}: signature needs an offsets=<file>:<count> component"));
    let (fname, count) =
        offsets_part.rsplit_once(':').expect("offsets component must be <file>:<count>");
    let count: u64 = count.parse().expect("offsets count must be numeric");
    assert_eq!(count, total, "{name}: signature offsets count must equal total");
    let manifest = workspace_root().join("scripts/bench/lme/questions").join(fname);
    let rows = manifest_rows(&manifest);
    assert_eq!(
        rows as u64,
        total,
        "{} holds {rows} questions but {name} claims {total} — manifest drift; \
         re-bless the baseline",
        manifest.display()
    );

    // `operating_point` and `smallest_detectable_regression_points` are DERIVED
    // fields — tally.py computes both from the signature and the counts
    // (scripts/bench/lme/tally.py:246-248). They are also plain JSON anyone can
    // hand-edit, and a floor that misreports its own sensitivity is worse than
    // no floor at all. Re-derive both and compare.
    let rerank = signature
        .split('|')
        .find_map(|part| part.strip_prefix("rerank="))
        .unwrap_or_else(|| panic!("{name}: signature carries no rerank= component"));
    let derived_point = match rerank {
        "0" => "fast",
        "1" => "quality",
        other => panic!("{name}: unexpected rerank={other} in signature (expected 0 or 1)"),
    };
    let stated_point =
        v["operating_point"].as_str().unwrap_or_else(|| panic!("{name}: operating_point missing"));
    assert_eq!(
        stated_point, derived_point,
        "{name}: operating_point says {stated_point}, but the config signature says \
         rerank={rerank} ({derived_point}) — one of the two was hand-edited, and the \
         published numbers are labelled from this field"
    );

    let claimed = v["smallest_detectable_regression_points"]
        .as_f64()
        .unwrap_or_else(|| panic!("{name}: smallest_detectable_regression_points missing"));
    let computed = 100.0 * (tolerance + 1) as f64 / total as f64;
    assert!(
        (claimed - computed).abs() < 0.05,
        "{name}: claims a {claimed:.1}-point smallest detectable regression, but \
         tolerance {tolerance} over N={total} actually gives {computed:.1}. The gate \
         is blinder than the file says it is."
    );

    Baseline { total, tolerance, signature }
}

/// Every baseline in the tree — legacy and gating alike — must parse and agree
/// with its manifest.
#[test]
fn every_baseline_is_coherent_with_its_offsets_manifest() {
    for name in std::iter::once(LEGACY_BASELINE).chain(GATING_BASELINES.iter().copied()) {
        load_and_check_baseline(name);
    }
}

/// The gate must be able to fail on a regression of the size this project
/// actually argues about. At N=16 / tolerance=1 the smallest detectable drop
/// was 2/16 = 12.5 points, so a 5-point retrieval regression — the scale of
/// the deltas in `docs/benchmarks/` — was invisible.
#[test]
fn gating_baselines_can_detect_a_five_point_regression() {
    for name in GATING_BASELINES {
        let b = load_and_check_baseline(name);
        let blind_spot = (b.tolerance + 1) as f64 / b.total as f64 * 100.0;
        assert!(
            b.total >= MIN_N_PER_TOLERANCE_STEP * (b.tolerance + 1),
            "{name}: tolerance {} over N={} means the gate cannot see a regression \
             smaller than {blind_spot:.1} points (limit {MAX_BLIND_SPOT_POINTS}). \
             Buy sensitivity with N, not by dropping the tolerance — the tolerance is \
             a correctness allowance for cross-platform float math, and a gate that \
             cries wolf gets disabled.",
            b.tolerance,
            b.total
        );
    }
}

/// Two baselines that both measure the same operating point would let CI
/// report "both points gated" while gating one — the published `fast` default
/// would stay exactly as unguarded as it was before the split.
#[test]
fn gating_baselines_cover_two_distinct_operating_points() {
    let points: Vec<(&str, String)> = GATING_BASELINES
        .iter()
        .map(|name| {
            let b = load_and_check_baseline(name);
            let rerank = b
                .signature
                .split('|')
                .find_map(|part| part.strip_prefix("rerank="))
                .unwrap_or_else(|| panic!("{name}: signature carries no rerank= component"))
                .to_string();
            (*name, rerank)
        })
        .collect();

    let fast = points.iter().filter(|(_, r)| r == "0").count();
    let quality = points.iter().filter(|(_, r)| r == "1").count();
    assert!(
        fast == 1 && quality == 1,
        "the gating pair must be exactly one `fast` (rerank=0, the SHIPPED default) \
         and one `quality` (rerank=1), got {points:?}. Gating the same point twice \
         reads as coverage and is not."
    );

    // A `fast` baseline whose filename says `quality` (or vice versa) would pass
    // the count check above while pointing CI at the wrong floor.
    for (name, rerank) in &points {
        let expected = if rerank == "0" { "fast" } else { "quality" };
        assert!(
            name.contains(expected),
            "{name} measures rerank={rerank} ({expected}) but its filename says otherwise — \
             the workflow selects the baseline BY NAME, so this mismatch silently gates \
             each arm against the other arm's floor"
        );
    }
}

fn manifest_rows(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .count()
}

/// The gauntlet must stay dead. Its yml resurfacing would re-register a
/// workflow that either startup-fails on every push or queues forever — both
/// alarm-fatigue generators that hid real breakage for weeks.
#[test]
fn the_eval_gauntlet_workflow_stays_deleted() {
    let p = workspace_root().join(".github/workflows/eval-gauntlet.yml");
    assert!(
        !p.exists(),
        "{} is back. The eval gauntlet was replaced by recall-ratchet.yml (GA-2a); \
         if a heavier LLM-judged gate is wanted again, build it as a NEW workflow \
         that is provably schedulable, and keep this tombstone until then",
        p.display()
    );
}

/// A gate that gets cancelled reports nothing, and "reported nothing" is
/// indistinguishable from "passed" on a board with no branch protection.
/// That is the exact failure mode STARTUP-01 hid behind for weeks, arriving
/// by a different road.
///
/// Observed 2026-08-20: `recall-ratchet` was `cancelled` on the two most
/// recent main pushes (2026-08-19 07:03Z and 08:48Z), leaving main's recall
/// quality unmeasured since 05:26Z. Cause: this is the ONLY main-push
/// workflow carrying a `concurrency:` block, and its group keyed on
/// `github.ref` — which is `refs/heads/main` for EVERY main push. With
/// `cancel-in-progress: true`, commit N+1 cancels commit N's in-flight run,
/// and the ratchet's ~40-minute wall clock means any merge train voids it.
///
/// The fix is to key the group on `github.sha` as well, so a run can only
/// ever cancel another run **of the same commit** (a genuine duplicate,
/// which is what cancel-in-progress is for). Every distinct main commit then
/// gets measured.
///
/// NOTE the residual, deliberately not papered over: nothing *detects* a
/// ratchet that never ran, because `main` has no branch protection and so no
/// required checks. Closing that needs an owner decision, not a code change;
/// this guard closes the cause instead of the symptom.
#[test]
fn recall_ratchet_concurrency_cannot_cancel_a_different_commit() {
    let yml = workflow_src();

    let group = yml
        .lines()
        .skip_while(|l| l.trim_start() != "concurrency:")
        .find(|l| l.trim_start().starts_with("group:"))
        .unwrap_or_else(|| {
            panic!(
                "recall-ratchet.yml has no `concurrency.group`. If the whole \
                 concurrency block was removed that is ACCEPTABLE (every run \
                 then completes) — delete this test deliberately rather than \
                 letting it fail silently."
            )
        })
        .to_string();

    let cancels = yml
        .lines()
        .skip_while(|l| l.trim_start() != "concurrency:")
        .any(|l| l.trim_start() == "cancel-in-progress: true");

    if cancels {
        assert!(
            group.contains("github.sha"),
            "recall-ratchet.yml sets `cancel-in-progress: true` with group \
             `{}`, which does not include `github.sha`. On main every push \
             shares one group key, so a new commit CANCELS the previous \
             commit's ratchet and that commit ships unmeasured. Include \
             github.sha in the group so a run can only supersede a duplicate \
             of the SAME commit.",
            group.trim()
        );
    }
}

/// The workflow must gate EXACTLY the baselines this guard validates.
///
/// Without this, narrowing the matrix to `point: [fast]` would leave CI gating
/// one arm while the guard happily kept validating two files — coverage on
/// paper, one measured arm in fact. Both the measure matrix and the ratchet
/// matrix are checked: measuring both arms but ratcheting only one is the same
/// hole wearing a different hat.
#[test]
fn workflow_gates_exactly_the_baselines_this_guard_validates() {
    let yml = workflow_src();
    assert!(yml.contains(BASELINE_PATH_TEMPLATE), "baseline path template missing");

    let axes: Vec<Vec<String>> = yml
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("point: ["))
        .map(|l| {
            l.trim_start_matches("point: [")
                .trim_end_matches(']')
                .split(',')
                .map(|p| p.trim().to_string())
                .collect()
        })
        .collect();

    assert!(
        axes.len() >= 2,
        "expected a point axis on BOTH measure and ratchet, found {}. Measuring two arms and ratcheting one gates nothing on the unratcheted arm.",
        axes.len()
    );
    for (i, a) in axes.iter().enumerate() {
        assert_eq!(
            a, &axes[0],
            "point axis #{i} differs from the first: the jobs would measure and ratchet different sets of arms"
        );
    }

    let mut got: Vec<String> = axes[0].iter().map(|p| format!("ci-anygold-{p}-n40.json")).collect();
    got.sort();
    let mut want: Vec<String> = GATING_BASELINES.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(
        got, want,
        "recall-ratchet.yml's matrix points expand to a different baseline set than GATING_BASELINES. CI and this guard must agree on which arms are gated."
    );
}

// ---------------------------------------------------------------------------
// The third road to "reported nothing" — a job TIMEOUT.
//
// `recall_ratchet_concurrency_cannot_cancel_a_different_commit` above closed
// the second road (a merge train cancelling the previous commit's run) and
// explicitly named the shape: *a gate that gets cancelled reports nothing, and
// "reported nothing" is indistinguishable from "passed" on a board with no
// branch protection.* A job that exceeds `timeout-minutes` is reported by
// GitHub as `cancelled` too — same word, same indistinguishable board, a
// cause the previous fix does not touch.
//
// Measured on run 32484079897 (2026-08-21, the first and only run of the
// two-arm N=40 shape, commit dc11319):
//
//   arm      setup+build   measure          per question   outcome
//   fast     7m07s         46m02s / 10 q    4.60 min       success, 53m total
//   quality  8m02s         cut at 62m10s    ~12.4 min      CANCELLED at 70m
//
// All FOUR quality shards completed exactly 5 of their 10 questions and died
// inside the 6th — not a flake, a structural impossibility. `quality` turns
// the reranker on, and a CPU cross-encoder pass costs ~2.7x the fast arm per
// question (one measured question: `duration_ms: 675980`, 11.3 minutes).
// 10 questions x 12.4 + 8 = ~132 minutes against a 70-minute ceiling.
//
// So the `quality` arm — the arm every published recall number was measured
// on — has never once produced a ratchet verdict at N=40, and the board said
// `cancelled`, not red.
// ---------------------------------------------------------------------------

/// Measured minutes per question on the hosted CPU runner, worst arm.
/// From run 32484079897: `quality` completed 5 questions in 62m10s across all
/// four shards (12.4 min/q); one question's own record says 11.3 min. Take the
/// slower figure — the budget must hold for the arm that costs the most.
const MEASURED_MIN_PER_QUESTION: usize = 13;

/// Setup + Moon build + lunaris-evals build, before the first question runs.
/// Observed 7m07s (warm caches) and 8m02s (GGUF download not cached). Budget
/// generously: a cold `rust-cache` miss rebuilds llama.cpp from scratch.
const BUILD_BUDGET_MIN: usize = 25;

fn measure_timeout_minutes(yml: &str) -> usize {
    yml.lines()
        .map(str::trim)
        .find(|l| l.starts_with("timeout-minutes:"))
        .and_then(|l| l.trim_start_matches("timeout-minutes:").trim().parse().ok())
        .expect("recall-ratchet.yml measure job has no parseable `timeout-minutes:`")
}

/// The shard axis, e.g. `shard: [0, 1, 2, 3]` -> 4.
fn matrix_shard_count(yml: &str) -> usize {
    let line = yml
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("shard: ["))
        .expect("recall-ratchet.yml has no `shard: [...]` matrix axis");
    line.trim_start_matches("shard: [")
        .trim_end_matches(']')
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .count()
}

/// The wall-clock budget must cover the work actually asked of a shard.
///
/// This is the guard that would have gone red at `5e37105` ("ratchet BOTH
/// operating points at N=40, not one arm at N=16"). That commit multiplied the
/// manifest by 2.5x AND added a second arm costing 2.7x per question, while
/// leaving `timeout-minutes: 70` — a number derived for 4 questions of the
/// cheap arm. The workflow's own comment still described the old shape ("~4
/// questions x ~2-6 min ... warm-cache target well under 40 min"), so nothing
/// in review contradicted it.
///
/// Deriving the budget from the manifest is the point: change N, change the
/// shard count, or add a costlier arm, and this recomputes instead of trusting
/// a number somebody typed once.
#[test]
fn the_shard_timeout_covers_the_work_the_matrix_asks_for() {
    let yml = workflow_src();
    let questions =
        manifest_rows(&workspace_root().join("scripts/bench/lme/questions/offsets40.tsv"));
    let shards = matrix_shard_count(&yml);
    let per_shard = questions.div_ceil(shards);
    let needed = per_shard * MEASURED_MIN_PER_QUESTION + BUILD_BUDGET_MIN;
    let budget = measure_timeout_minutes(&yml);

    assert!(
        budget >= needed,
        "recall-ratchet's measure job allows {budget} min, but {questions} questions \
         over {shards} shards is {per_shard} questions/shard = {} min of measuring \
         (at {MEASURED_MIN_PER_QUESTION} min/q on the QUALITY arm, measured) plus \
         {BUILD_BUDGET_MIN} min of build = {needed} min. A shard that runs out of \
         clock is reported by GitHub as `cancelled`, NOT as a failure, so the arm \
         ships unmeasured behind a board that never turned red — the same \
         indistinguishable-from-passing shape this file already guards twice. \
         Either raise timeout-minutes or add shards; do not shrink the manifest \
         (N=40 is what buys the 5-point detection floor).",
        per_shard * MEASURED_MIN_PER_QUESTION
    );
}

/// One arm's shard dying must not silently void the OTHER arm's verdict.
///
/// `ratchet` declares `needs: measure`, and `needs` waits on the ENTIRE measure
/// matrix — both arms, every shard. On run 32484079897 all four `fast` shards
/// passed and uploaded their artifacts; the four `quality` shards timed out;
/// and BOTH ratchet jobs were `skipped`. The fast arm's numbers were sitting in
/// artifact storage, complete and uncompared.
///
/// The fix is `if: ${{ !cancelled() }}` on the fan-in: run whenever the
/// workflow itself was not cancelled, and let the job's own "expected N shard
/// config files" assertion decide. That assertion already exists and is
/// already correct — it simply never got to run. An arm with all its shards
/// then ratchets normally; an arm missing shards goes RED and says which.
///
/// `!cancelled()` rather than `always()` deliberately: a human (or a
/// same-commit concurrency supersede) cancelling the whole run should still
/// stop the fan-in, not convert a deliberate cancel into a red board.
#[test]
fn a_timed_out_shard_cannot_silently_skip_the_ratchet() {
    let yml = workflow_src();
    let ratchet = yml
        .split_once("  ratchet:")
        .map(|(_, rest)| rest)
        .expect("recall-ratchet.yml has no `ratchet:` job");
    // The job's own keys, before `steps:`.
    let header = ratchet.split_once("    steps:").map(|(h, _)| h).unwrap_or(ratchet);

    assert!(
        header.lines().any(|l| {
            let t = l.trim();
            t.starts_with("if:") && (t.contains("!cancelled()") || t.contains("always()"))
        }),
        "the `ratchet` fan-in job has no `if:` that survives a non-success in \
         `measure`. With a bare `needs: measure` a single timed-out shard in \
         EITHER arm skips BOTH ratchet jobs — observed on run 32484079897, where \
         four green `fast` shards had already uploaded complete artifacts and \
         were never compared to their baseline. A skipped gate reports nothing, \
         and nothing is indistinguishable from a pass. Add `if: ${{{{ \
         !cancelled() }}}}` and let the job's existing shard-count assertion \
         turn the missing arm red."
    );
}

/// The shard count is written in three places that must agree.
///
/// 1. the matrix axis `shard: [...]` — how many jobs run
/// 2. `SHARD_COUNT` in the measure step's env — how the gate script slices the
///    manifest
/// 3. the ratchet's `[ "$(ls merged/config.shard*.env | wc -l)" -eq N ]` — how
///    many shard results the fan-in demands before it will compare
///
/// Raising the shard count and missing (2) makes `anygold_gate.sh` reject the
/// out-of-range index loudly (it checks `SHARD_INDEX < SHARD_COUNT`), so that
/// half is already safe. Missing (3) makes every run red with a confusing
/// message. Pin all three to one number so the next change to shard geometry is
/// a single edit and a green test, not a guess.
#[test]
fn the_shard_count_agrees_across_matrix_script_env_and_fan_in() {
    let yml = workflow_src();
    let matrix = matrix_shard_count(&yml);

    let env_count: usize = yml
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("SHARD_COUNT:"))
        .and_then(|l| l.trim_start_matches("SHARD_COUNT:").trim().trim_matches('"').parse().ok())
        .expect("recall-ratchet.yml has no parseable `SHARD_COUNT:`");
    assert_eq!(
        matrix, env_count,
        "the matrix runs {matrix} shards but SHARD_COUNT tells anygold_gate.sh to \
         slice the manifest {env_count} ways. The script refuses an out-of-range \
         index, so this fails the job — loudly, but with a message about the \
         script rather than about the workflow that is actually wrong."
    );

    let fan_in: usize = yml
        .lines()
        .map(str::trim)
        .find(|l| l.contains("config.shard*.env") && l.contains("-eq"))
        .and_then(|l| l.rsplit("-eq").next()?.split(']').next()?.trim().parse().ok())
        .expect("recall-ratchet.yml's ratchet job has no parseable shard-count check");
    assert_eq!(
        matrix, fan_in,
        "the matrix runs {matrix} shards but the fan-in demands exactly {fan_in} \
         shard config files before it will compare against the baseline."
    );
}
