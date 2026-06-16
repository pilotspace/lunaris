# TASK: Un-stub LoCoMo/LongMemEval/ER-F1 harnesses; make CI gate on real scores

slug: eval-gauntlet-ci-gate · created: 2026-06-15 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it. -->
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower the
     autonomy level to `manual` or `conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

> Verified via serena/grep 2026-06-16. The runner + workflow + SKIP-clean shells already EXIST;
> the gap is the SCORING CORE (parsers + metric math), not the plumbing.

Touches (files · symbols · signatures):
  - `crates/lunaris-bench/src/eval/er_f1.rs:run` — ER-F1 harness; line 90 `let f1 = 0.0` stub → `judge_ge(0.0, 0.80)` = FAIL on a live run. Needs WNUT-2017 gold parse + extractor invocation + F1 math.
  - `crates/lunaris-bench/src/eval/longmemeval.rs:{ingest_corpus_and_collect_queries,compute_j_score}` — reference harness; parser returns `Vec::new()`, J-score returns `0.0` (line 223). Recall-J-score = % queries whose top-k contains the gold answer.
  - `crates/lunaris-bench/src/eval/locomo.rs:run` — same shape as longmemeval; line 101 `let j_score = 0.0` stub.
  - `crates/lunaris-bench/src/eval/mod.rs:EvalRow::{judge_ge,judge_le,skipped}` — row + PASS/FAIL/SKIPPED verdict (unchanged; the gate primitive).
  - `crates/lunaris-bench/src/bin/evals.rs:main` — runner; `lunaris-evals all` writes eval-results.json, exits 1 on any `"status":"FAIL"` (the gate mechanism — already correct).
  - `.github/workflows/eval-gauntlet.yml` — gate workflow on `ubuntu-latest` + `services: moondb/moon:latest`. PROBLEM: GHA `services:` can't run an unpublished/locally-built image (CONVENTIONS — use the integration.yml manual docker-run pattern) AND ubuntu-latest has no model weights → er-f1 always SKIPs → can never produce real numbers. Compare the working pattern: `.github/workflows/llm-gates.yml` runs on `[self-hosted, llm-weights-cached]`, SKIP-clean.
Context (working folder): the `eval-results.json` manifest (D-20 shape); the J/F1 numbers are HUMAN-UAT-deferred (need live Gemma weights + Moon + HF datasets, none CI-runnable) per [[feedback_moon_first_pg_deferred]]. This task ships the SCORING CODE + a SKIP-clean real gate + a seeded-regression discriminating test; the actual Mem0-comparable numbers populate at HUMAN-UAT on a weights-cached runner.
Honors (patterns / conventions): SKIP-clean soft-fail (D-21: missing env/data/model → SKIPPED, never FAIL); `#![forbid(unsafe_code)]` (no `std::env` mutation — split a pure scorer the unit test drives, env-reading wrapper covered e2e, per [[reference_moon_op_timeout_test_harness]]); built-≠-wired ([[feedback_built_not_wired]] — the live-pipeline arm is the real production code, proven by a structural guard + the pure scorer's red→green, run for real at HUMAN-UAT, mirroring `extractor-fallback-wiring`); the `llm-gates.yml` weights-cached self-hosted runner shape for the real gate.
Anchors the contract cites: pure scorers `compute_f1(pred,gold)` + `recall_j_score(per_query_topk_hits, gold)`; the dataset parsers (`parse_wnut`, `parse_longmemeval`, `parse_locomo`); the no-models-→-SKIP fix in each `run`; the seeded-regression gate test.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Real LoCoMo/LongMemEval/ER-F1 scoring (pure metric math + dataset parsers) wired into the harnesses so present-data computes a real score and absent-capability SKIPs (killing the 0.0→FAIL trap), plus a SKIP-clean gauntlet CI gate proven to fail on a seeded sub-threshold regression. Mem0-comparable numbers populate at HUMAN-UAT on a weights-cached runner.
Framings weighed: **scoring-core + skip-clean-gate** (chosen — un-stub the metric math, fix SKIP/FAIL, prove the gate bites; defer live numbers to a weights-cached runner) · gate-mechanism-only (rejected — leaves the scoring stubbed, doesn't "un-stub", keeps the 0.0→FAIL trap) · run-it-here-for-real-numbers (rejected — impossible in autonomous CI: no Gemma weights / Moon / HF datasets; numbers are a HUMAN-UAT deliverable per [[feedback_moon_first_pg_deferred]])
Must:
<must>
  - PURE `compute_f1(predicted, gold)` over `(entity,type)` pairs: micro-averaged set precision/recall/F1; no env/network/backend access; deterministic.
  - PURE `recall_j_score(per_query)` = 100 × (#queries whose top-k recalled text contains the normalized gold answer) ÷ #queries; used by BOTH longmemeval + locomo.
  - PURE dataset parsers (bytes → typed): `parse_wnut`, `parse_longmemeval`, `parse_locomo`; fixture-testable, no I/O of their own.
  - SKIP-not-FAIL: model weights unset/unloadable OR dataset absent/garbage OR backend unreachable → `EvalRow::skipped` (NEVER the current 0.0→FAIL). Data+model present → compute real score → `judge_ge`.
  - The runner gate bites: a manifest with any `"status":"FAIL"` → `lunaris-evals` exits 1; a PASS/SKIPPED-only manifest → exits 0 (assert via a discriminating test, not just trust the existing `main`).
  - `eval-gauntlet.yml` becomes a REAL SKIP-clean gate: `runs-on: [self-hosted, llm-weights-cached]` + Moon via the integration.yml manual docker-run pattern (NOT `services: moondb/moon:latest`); green-by-skip on runners without weights.
  - `#![forbid(unsafe_code)]`: no `std::env` mutation; pure scorers unit-driven, env-reading arms covered e2e. No new HNSW/BM25/eval libs.
</must>
Reject:
<reject>
  - A harness emitting `FAIL` when models/data are merely ABSENT (the 0.0→FAIL trap) -> "false_fail_on_absent"
  - `compute_f1`/`recall_j_score` reading env, hitting network, or touching a backend (not pure) -> "impure_scorer"
  - A non-SKIPPED row whose score was not computed over actually-parsed data -> "vacuous_score"
  - A workflow `services:` entry pointing at an unpublished/locally-built Moon image -> "unrunnable_service"
  - A J/F1 number quoted as a Lunaris result without the HUMAN-UAT-on-weights-cached methodology note -> "apples_to_oranges"
</reject>
After:
<after>
  - `compute_f1` + `recall_j_score` + the 3 parsers exist, pure, fixture-unit-tested (red→green).
  - Each harness `run` computes a real score when data+model present and SKIPs otherwise; the 0.0→FAIL trap is gone.
  - A discriminating test proves the runner exits 1 on a seeded sub-threshold FAIL and 0 on PASS/SKIPPED-only.
  - `eval-gauntlet.yml` is a SKIP-clean real gate on the weights-cached runner; numbers populate at HUMAN-UAT.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ [contract] SCOPE BREADTH + NUMBER DEFERRAL — this ships the Rust scoring core + parsers + SKIP/FAIL fix + gate-regression test AND reworks `eval-gauntlet.yml` to the weights-cached pattern, with the actual Mem0-comparable NUMBERS deferred to HUMAN-UAT. Lowest confidence because it decides how much lands now vs at UAT; if you expected live numbers in-session that is impossible here (no weights/Moon/datasets) and exit-criterion #3 closes as "mechanism + scoring shipped; numbers pending HUMAN-UAT", not fully green. Mitigation: confirm at freeze; deferral is a standing decision.
  - [ ] J-score = recall-based (top-k contains normalized gold), NOT an LLM-judge — the autonomously-computable proxy; the LLM-judge free-form variant is a HUMAN-UAT refinement. Confirm the proxy is acceptable for the gate.
  - [ ] ER-F1 = micro-averaged over (entity,type) with case/whitespace-normalized exact-match (vs token-level/partial). Confirm at contract.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: compute_f1 is correct on a known pred/gold set
  Given predicted (entity,type) pairs and gold pairs with a known overlap
  When compute_f1(predicted, gold) runs
  Then it returns the micro-averaged F1 (2PR/(P+R)) for that overlap
  And it touches no env var, network, or backend (pure)

Scenario: compute_f1 handles empty inputs without NaN
  Given empty gold AND empty predicted
  When compute_f1 runs
  Then it returns 1.0 (vacuously perfect); empty-one-side returns 0.0
  And it never returns NaN or panics

Scenario: recall_j_score counts top-k gold hits
  Given N queries, K of which have a top-k text containing the normalized gold answer
  When recall_j_score(per_query) runs
  Then it returns 100*K/N
  And an empty query set returns 0.0 (not NaN)

Scenario: parsers turn dataset bytes into typed records
  Given the committed sample fixture bytes for WNUT / LongMemEval / LoCoMo
  When parse_wnut / parse_longmemeval / parse_locomo run on them
  Then each returns the expected typed records (gold pairs / query+answer)
  And malformed bytes return Err (caller maps to SKIPPED), never a partial-silent parse

Scenario: harness SKIPs when capability is absent  (Reject: false_fail_on_absent)
  Given model weights unset OR dataset missing OR backend unreachable
  When er_f1::run / longmemeval::run / locomo::run executes
  Then it pushes an EvalRow with status "SKIPPED"
  And it NEVER pushes status "FAIL" merely because data/model were absent

Scenario: harness computes a real score when data+model are present
  Given a fixture-injected parsed dataset and a stub scorer input
  When the harness scoring path runs
  Then the row's value is the computed score (not a hardcoded 0.0)
  And the row is judged via judge_ge against the threshold

Scenario: the runner gate bites on a sub-threshold regression
  Given an eval-results.json containing any row with "status":"FAIL"
  When the gate check runs over the manifest
  Then it reports failure (exit 1)
  And a manifest of only PASS/SKIPPED rows reports success (exit 0)

Scenario: scorers are pure by signature  (Reject: impure_scorer)
  Given the compute_f1 / recall_j_score definitions
  When their source is inspected
  Then neither reads std::env, performs I/O, or takes a backend handle
  And they live in a dedicated pure module (eval/score.rs)

Scenario: the CI workflow is runnable, not a phantom service  (Reject: unrunnable_service)
  Given .github/workflows/eval-gauntlet.yml
  When its source is inspected
  Then it has NO `services:` entry referencing an unpublished Moon image
  And it targets a weights-cached runner and is green-by-skip without weights
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
NEW PURE MODULE  crates/lunaris-bench/src/eval/score.rs   (#![forbid(unsafe_code)]; no env/IO/backend)
  pub fn compute_f1(predicted: &[(String, String)], gold: &[(String, String)]) -> f64
    // micro set-overlap: tp=|pred∩gold|, P=tp/|pred|, R=tp/|gold|, F1=2PR/(P+R)
    // |gold|==0 && |pred|==0 -> 1.0 ; exactly one side empty -> 0.0 ; never NaN
    pub struct QueryHit { pub topk_texts: Vec<String>, pub gold_answer: String }
  pub fn recall_j_score(per_query: &[QueryHit]) -> f64
    // 100.0 * (#queries where any topk_text contains normalize(gold_answer)) / len ; empty -> 0.0
    // normalize = lowercase + trim + collapse-whitespace (shared helper)

PARSERS (pure; bytes -> typed; Err on malformed so the caller maps to SKIPPED)
  er_f1.rs::parse_wnut            : &[u8] -> anyhow::Result<Vec<WnutDoc>>     WnutDoc { text:String, gold:Vec<(String,String)> }
  longmemeval.rs::parse_longmemeval : &[u8] -> anyhow::Result<Vec<EvalQuery>>   // fills the existing EvalQuery
  locomo.rs::parse_locomo         : &[u8] -> anyhow::Result<Vec<EvalQuery>>

HARNESS CONTRACT  (er_f1::run · longmemeval::run · locomo::run)
  weights/env unset  -> EvalRow::skipped   |  dataset missing/garbage -> EvalRow::skipped
  backend unreachable -> EvalRow::skipped   |  model load fails        -> EvalRow::skipped
  data+model present  -> compute real score via score:: fn -> EvalRow::judge_ge(score, THRESHOLD)
  INVARIANT: a harness NEVER emits status "FAIL" for a merely-absent capability (kills 0.0->FAIL)

RUNNER GATE  (crates/lunaris-bench/src/bin/evals.rs — behavior unchanged, now TESTED)
  any row "status":"FAIL" -> ExitCode 1   |   only PASS/SKIPPED -> ExitCode 0

CI WORKFLOW  .github/workflows/eval-gauntlet.yml
  runs-on: [self-hosted, llm-weights-cached]   (NOT ubuntu-latest)
  Moon: manual `docker run` step (integration.yml pattern)   (NOT a services moondb image)
  SKIP-clean: env unset on a bare runner -> all harnesses SKIPPED -> gate green ; real numbers @ HUMAN-UAT

TESTS  (red-first)
  eval/score.rs #[cfg(test)]                              — compute_f1 + recall_j_score unit cases
  crates/lunaris-bench/tests/eval_scoring.rs              — parser fixtures + harness SKIP-not-FAIL + score-when-present
  crates/lunaris-bench/tests/eval_gate.rs                 — seeded FAIL manifest -> exit 1 ; PASS/SKIPPED-only -> exit 0
  crates/lunaris-bench/tests/eval_workflow_guard.rs       — structural: eval-gauntlet.yml has no services-Moon-image + weights-cached label
  committed fixtures under crates/lunaris-bench/tests/fixtures/eval/{wnut,longmemeval,locomo}.sample.json
```

Status: FROZEN @ v1 — approved by Tin Dang 2026-06-16. Least-sure flag surfaced at freeze: [contract] scope breadth + number deferral — the scoring code + SKIP/FAIL fix + gate test + workflow rework ship now; the actual Mem0-comparable J/F1 NUMBERS are HUMAN-UAT-deferred (no Gemma weights / Moon / HF datasets in autonomous CI). Confirmed: J=recall-proxy (top-k contains normalized gold, NOT LLM-judge); ER-F1=micro exact-match over normalized (entity,type). Changing this contract = change request back to SPECIFY.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: 100% of the §1 Musts + Rejects (each has ≥1 discriminating test); pure scorers fully branch-covered.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_compute_f1_known_set: arrange pred/gold with 2 of 3 overlap / act compute_f1 / assert F1 == expected (e.g. 0.8) ; assert no env/IO
  - test_compute_f1_empty: arrange (empty,empty)->1.0 ; (empty,nonempty)->0.0 ; (nonempty,empty)->0.0 / assert never NaN, no panic
  - test_recall_j_score_fraction: arrange 4 queries, 3 with top-k containing gold / act recall_j_score / assert == 75.0 ; empty -> 0.0
  - test_parsers_roundtrip: arrange committed sample fixtures / act parse_wnut/parse_longmemeval/parse_locomo / assert expected typed records ; malformed bytes -> Err
  - test_harness_skips_on_absent: arrange env unset / act each run() / assert exactly one row, status SKIPPED, NEVER FAIL  (Reject: false_fail_on_absent)
  - test_harness_scores_when_present: arrange fixture-injected parsed data + scorer input / act the scoring path / assert row.value == computed score (not 0.0), judged via judge_ge
  - test_gate_bites_on_fail: arrange eval-results.json with a FAIL row / act the gate check / assert exit 1 ; PASS+SKIPPED-only manifest -> exit 0
  - test_scorer_purity_guard: structural — assert score.rs has no `std::env`, no backend import (Reject: impure_scorer)
  - test_workflow_runnable_guard: structural — assert eval-gauntlet.yml has no services-Moon-image + targets llm-weights-cached (Reject: unrunnable_service)
</test_plan>

Tests live in: `crates/lunaris-bench/src/eval/score.rs` (in-module unit) · `crates/lunaris-bench/tests/eval_scoring.rs` · `crates/lunaris-bench/tests/eval_gate.rs` · `crates/lunaris-bench/tests/eval_workflow_guard.rs` · fixtures `crates/lunaris-bench/tests/fixtures/eval/` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-bench/src/eval/` · `crates/lunaris-bench/tests/` · `.github/workflows/eval-gauntlet.yml`
Strategy (ordered batches): 1. new `eval/score.rs` pure module (compute_f1 + recall_j_score + normalize) + in-module tests RED→GREEN. 2. parsers (`parse_wnut`/`parse_longmemeval`/`parse_locomo`) + fixtures + `eval_scoring.rs` RED→GREEN. 3. wire each `run()` to parse→score→judge with SKIP-not-FAIL on every absent-capability path. 4. `eval_gate.rs` seeded-regression test (manifest→exit code). 5. rework `eval-gauntlet.yml` to weights-cached + manual-docker-Moon + `eval_workflow_guard.rs`. 6. full suite green + clippy/fmt.
Safety rule (feature-specific): SKIP-not-FAIL is the invariant — every error/absence path returns `EvalRow::skipped`, never a 0.0→FAIL row; pure scorers take NO env/backend.
Code lives in: `crates/lunaris-bench/src/eval/` + `crates/lunaris-bench/tests/`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build
- [ ] the green was EARNED, not gamed — no overfit to fixtures, vacuous asserts, or stubbed-away logic (score with an adversarial refute-read — a subagent recommended under `autonomy: auto`; a confirmed cheat is HARD-STOP)
- [ ] concurrency / timing of the risky operation is safe
- [ ] no exposed secrets, injection openings, or unexpected dependencies
- [ ] layering & dependencies follow CONVENTIONS.md
- [ ] a person reviewed and approved the change

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [ ] WIRING (code) — every new symbol is referenced; record where / how confirmed
- [ ] DEAD-CODE (code) — no new unused or orphaned symbol introduced
- [ ] SEMANTIC (prose / non-code) — read in full, not skimmed: <what read · what confirmed>

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
If RISK-ACCEPTED -> owner: <name> · ticket: <link> · expires: <date>   (never for a security gap)
Reviewed by: <name> · date: <date>

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
