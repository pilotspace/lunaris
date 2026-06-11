# TASK: Repair conformance-bindings maturin venv failure

slug: ci-bindings-venv-fix · created: 2026-06-11 · stage: production
phase: build   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Make the conformance-bindings workflow (.github/workflows/conformance-bindings.yml) actually run: both jobs (`per-driver-parity` ×2 matrix rows + `memory-smoke`) die at "Build + install lunaris-py WITH bindings-it" because `maturin develop` requires a virtualenv and the workflow never creates one.

Ground facts (2026-06-11):
  - The workflow has NEVER passed: 62 failures + 1 cancelled in its entire run history back to creation 2026-04-23 (Plan 08-04). This is a constitutional defect, not a regression — "broken since 06-08" from the PR-#20 review was just the earliest sampled run.
  - Exact error, both jobs: `💥 maturin failed — Caused by: Couldn't find a virtualenv or conda environment…` at lines 88/169 (`maturin develop --release --features bindings-it`, working-directory crates/lunaris-py). setup-python@v5 provides a bare interpreter; VIRTUAL_ENV is unset.
  - Local mirror of the same class: plain `maturin develop` fails in our uv venv too; we run `maturin develop --uv` locally (CONVENTIONS/memory).
  - Downstream steps never executed in ANY run: the Rust/Python/TS parity steps are unexercised. Fixing the venv may UNMASK new failures (e.g. live-PG parity hitting paths the cypher(cstring) class touches, or drift in test_backend_parity.py / backend_parity.spec.mts that nobody has ever run in CI).
  - The moon matrix row is gated on `secrets.MOON_IMAGE` and currently no-op-skips when unset — its "failure" today is the maturin step, which runs BEFORE the skip gate.
  - Only backend_parity.spec.mts runs here; no workflow anywhere runs the full TS suite (task-2 scope).

Framings weighed: explicit `python -m venv` step + GITHUB_ENV/GITHUB_PATH export (chosen — minimal, no new tooling, fixes both jobs symmetrically, subsequent pytest steps inherit the venv) · `maturin build` + `pip install <wheel>` (maturin's own suggestion; rejected — changes install semantics and loses develop-mode parity with local flow) · adopt uv in CI (`uv venv` + `maturin develop --uv`; attractive for speed but a bigger toolchain decision than this repair warrants — noted as a future option, not this task).
Scope boundary: .github/workflows/conformance-bindings.yml ONLY. Unmasked downstream failures inside the same jobs are IN scope to triage; fixes for them are in scope only if ≤ trivial (else: recorded verbatim + split to a task, per the bench-rerun "report, don't massage" precedent). No crates/ changes expected.
Must:
<must>
  - Both jobs gain a venv before any pip/maturin step: `python -m venv .venv` + persist `VIRTUAL_ENV` and PATH via GITHUB_ENV/GITHUB_PATH so `pip install maturin…`, `maturin develop`, and `pytest` all use it
  - All three job instances (per-driver parity postgres, per-driver parity moon, memory-smoke) reach PAST the maturin step on a real CI run
  - Exit criterion run: workflow green on this task's PR (and on main after merge) OR any post-maturin failure recorded verbatim in §6 with a split-task proposal
</must>
Reject:
<reject>
  - weakening the workflow to get green (deleting parity steps, adding continue-on-error, narrowing triggers) -> contract violation, never
  - pinning an old maturin to dodge the venv requirement -> rejected: maturin develop has ALWAYS required a venv; there is no old-good version to pin
</reject>
After:
<after>
  - the per-driver parity gates (Rust/Python/TS × postgres/moon-skip) actually execute for the first time since the workflow was written
  - task ts-specs-v04-refresh has a working CI home for the full TS suite
  - the "CI never caught it" class of binding drift (stale TS specs) can no longer accumulate silently
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ The never-executed parity steps will pass once reached — lowest confidence because they have ZERO run history; the postgres row exercises live-PG paths adjacent to the known cypher(cstring) conformance failure. If wrong: the workflow stays red for a NEW reason — handled by the §1 scope rule (triage; trivial fix or verbatim record + split), not by weakening.
  - [x] The venv mechanism itself works on ubuntu-latest runners — python -m venv is stdlib; GITHUB_ENV/GITHUB_PATH persistence across steps is documented GHA behavior.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: maturin step passes in all three job instances
  Given the workflow with the venv step on a CI runner
  When conformance-bindings runs (PR trigger or workflow_dispatch)
  Then "Build + install lunaris-py WITH bindings-it" succeeds in per-driver parity (postgres),
       per-driver parity (moon), and feature-build smoke
  And the parity/test step list is unchanged (no deleted steps, no continue-on-error)

Scenario: downstream steps execute for the first time
  Given the maturin step passes
  When the Rust/Python/TS parity steps run
  Then each either passes OR its failure is recorded verbatim in §6 with a split-task proposal
  And no test/step is weakened to force green

Scenario: moon row keeps its neutral skip
  Given secrets.MOON_IMAGE is unset
  When per-driver parity (moon) runs
  Then the build steps pass and the backend-resolve step skips the parity steps neutrally
  And the job concludes green (not failure)
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
DELIVERABLE (one file): .github/workflows/conformance-bindings.yml
  Both jobs (per-driver-parity, memory-smoke) gain, immediately after setup-python:
    - name: Create Python venv
      run: |
        python -m venv .venv
        echo "VIRTUAL_ENV=$PWD/.venv" >> "$GITHUB_ENV"
        echo "$PWD/.venv/bin" >> "$GITHUB_PATH"
  Everything else unchanged: step list, triggers, matrix, moon-row skip gate,
  parity commands, working-directories. No continue-on-error anywhere.
Evidence protocol:
  red  = the standing failure history (62/62 runs dead at the maturin step; latest
         main run 2026-06-11 fails with the venv error verbatim)
  green= a real CI run of THIS workflow on the task branch (PR touches the workflow
         file -> pull_request trigger fires) with all three job instances past the
         maturin step; full job green OR any post-maturin failure recorded verbatim
         in §6 + split-task proposal (scope rule from §1)
Schema: no code, no crates/ changes; CI-config only.
```

Status: FROZEN @ v1 — approved by Tin Dang (2026-06-11, freeze #1 of bindings-gate-hardening)
Least-sure flag surfaced at freeze:
  ⚠ [spec] The never-executed parity steps (Rust/Python/TS × postgres) have ZERO run history — once unblocked they may fail for new reasons (the postgres row runs live-PG paths adjacent to the known cypher(cstring) conformance failure). Cost if wrong: the workflow stays red for a new, now-visible reason; contracted response = triage, trivial-fix-or-record-verbatim + split task, never weaken.
  ⚠ [contract] `python -m venv` + GITHUB_ENV persistence is the chosen mechanism over uv; if the team later standardizes CI on uv this step gets replaced wholesale (cheap, single file) — deliberately NOT adopting uv here to keep the repair minimal.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: CI-config task — the executable test IS the workflow run itself (no cargo/pytest suite can exercise GHA step plumbing). Red is already standing: every run in history fails at the maturin step for the venv reason (latest: main 2026-06-11, `Couldn't find a virtualenv` verbatim in 3/3 job instances).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - scenario 1+3 (maturin passes ×3 jobs, moon row neutral-skip): the task-branch PR CI run of conformance-bindings — assert all three instances pass the maturin step; moon row green via skip gate
  - scenario 2 (downstream first-execution): same run — each parity step passes or §6 records the verbatim failure + split proposal
  - local pre-flight (cheap, before pushing): `python3 -m venv /tmp/cb-venv && VIRTUAL_ENV=/tmp/cb-venv PATH=/tmp/cb-venv/bin:$PATH pip install maturin && maturin develop --release --features bindings-it` in crates/lunaris-py — mirrors the exact step sequence the workflow will run
</test_plan>

Tests live in: `.github/workflows/conformance-bindings.yml` (the run is the test) · red state = standing 62-run failure history, re-confirmed on main 2026-06-11.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): <e.g. debit+credit in one atomic transaction>
Code lives in: `./src/`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build
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
