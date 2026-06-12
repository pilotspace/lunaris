# TASK: pyo3 0.26.0 RUSTSEC-2026-0176/0177: bump to patched minor across PyO3 SDK stack

slug: pyo3-rustsec-advisories · created: 2026-06-12 · stage: production
phase: specify   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: remediate the two cargo-deny-failing RUSTSEC advisories against
pyo3 0.26.0 so the CI `check` job's cargo_deny step is green again.

EVIDENCE (recorded 2026-06-12, CI run 27390271658 cargo_deny step):
  - RUSTSEC-2026-0176 — "Out-of-bounds read in `nth` / `nth_back` for
    `PyList` and `PyTuple` iterators" (pyo3 0.26.0)
  - RUSTSEC-2026-0177 — "Missing `Sync` bound on `PyCFunction::new_closure`
    closures" (pyo3 0.26.0)
  - `cargo update -p pyo3 --dry-run` finds NOTHING within the `0.26` semver
    range (no 0.26.x patch exists) — remediation requires a MINOR bump.
  - Latest pyo3 on crates.io: 0.29.0. Pinned stack that must move in
    lockstep: pyo3 0.26 + pyo3-async-runtimes 0.26 + pythonize 0.26
    (workspace Cargo.toml:277-278; pythonize via cargo tree).
  - This failure is repo-wide advisory-DB drift: it fails EVERY branch
    including main (first surfaced on PR #25, reproduced on its rerun);
    it is independent of any in-flight feature work.
  - Workspace impact surface: crate `lunaris-py` only (the PyO3 SDK).
    Project CLAUDE.md pins "Python 3.11+ (PyO3 0.26 baseline)" — the
    baseline note must be updated with the bump.

Framings weighed: bump pyo3 stack to the smallest minor that the advisories
list as patched, in lockstep (pyo3 + pyo3-async-runtimes + pythonize), and
update the CLAUDE.md baseline (chosen — actually removes the vulnerable
code) · add both IDs to deny.toml ignore with expiry comments (rejected as
primary — security findings are never auto-passed; acceptable ONLY as an
explicitly-approved stopgap if the bump turns out to be a breaking-API
project of its own) · pin cargo-deny advisory DB (rejected — hides every
future advisory, worse than the disease).

Must:
<must>
  - cargo_deny advisories check green on CI (both RUSTSEC IDs resolved by
    version, not ignored)
  - lunaris-py compiles + its maturin/pytest suite passes against the bumped
    stack (memory: exclude lunaris-py/lunaris-ts from cargo test --workspace;
    use scripts/sdk-real-evidence.sh or maturin+pytest)
  - pyo3-async-runtimes + pythonize bumped in lockstep (mixed pyo3 majors do
    not compile)
  - emit_py.rs codegen output still carries `#[pyclass(dict)]` without
    `unsendable` (user-pinned behavior; re-verify after bump)
  - CLAUDE.md "PyO3 0.26 baseline" constraint line updated to the new baseline
</must>
Reject:
<reject>
  - deny.toml ignore entries without a signed RISK-ACCEPTED record -> ADD
    security rule (HARD-STOP otherwise)
  - bumping pyo3 but leaving pyo3-async-runtimes/pythonize behind ->
    "mixed_pyo3_versions" compile failure
  - abi3-py311 / extension-module feature set silently changed -> SDK wheel
    contract breaks downstream
</reject>
After:
<after>
  - CI `check` is fully green again on main (modulo the documented
    pre-existing integration/perf failures)
  - the PyO3 baseline in CLAUDE.md names the new minor; no advisory ignores
    in deny.toml
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ the patched pyo3 minor (read the two advisory pages for the exact
    `patched` ranges before contract) compiles against pyo3-async-runtimes /
    pythonize releases that exist today — lowest confidence because the
    async-runtimes crate historically lags pyo3 minors by weeks; if wrong:
    the bump stalls and the deny.toml ignore stopgap needs a human-approved
    RISK-ACCEPTED with expiry.
  - [ ] pyo3 0.27→0.29 API churn touching lunaris-py is mechanical
    (Bound<'py> migration landed pre-0.26; codegen templates may need
    regeneration) — size at contract time by compiling against the candidate.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
# ── Must: advisory resolution by version ─────────────────────────────────────

Scenario: cargo-deny advisories clean after version bump
  Given workspace Cargo.toml pins pyo3 = "0.29", pyo3-async-runtimes = "0.29",
        and pythonize = "0.28" (highest available lockstep; see CONTRACT §3 note)
  When `cargo deny check advisories` runs against the refreshed Cargo.lock
  Then the check exits 0 with no RUSTSEC-2026-0176 or RUSTSEC-2026-0177 findings
  And no deny.toml ignore entry for either advisory ID is present

# ── Must: lunaris-py compiles and its test suite passes ──────────────────────

Scenario: lunaris-py compiles and pytest suite passes after bump
  Given the workspace pyo3 stack is bumped (as above)
  When `maturin develop` is run inside crates/lunaris-py/ and then
       `pytest lunaris-py/tests/` is run
  Then the build completes without error and all pytest tests pass
  And no test in the suite is skipped or marked xfail to paper over a compile error

# ── Must: lockstep bump — all three crates move together ─────────────────────

Scenario: no mixed-pyo3-version compile artefact
  Given a clean `cargo build -p lunaris-py` with the bumped workspace pins
  When cargo resolves the dependency graph
  Then the build succeeds without a "found two different crates named `pyo3`"
       or "conflicting versions of pyo3" error
  And Cargo.lock contains exactly one version of pyo3, one of pyo3-async-runtimes,
      and one of pythonize

# ── Must: emit_py.rs codegen output preserves pyclass(dict) without unsendable ─

Scenario: codegen still emits pyclass(dict) and never unsendable after pyo3 bump
  Given the bumped workspace compiles lunaris-codegen
  When `cargo run -p lunaris-codegen -- --emit py` regenerates generated.rs
  Then every `#[pyclass]` annotation in generated.rs contains `dict`
  And no `#[pyclass]` annotation in generated.rs contains the token `unsendable`

# ── Must: CLAUDE.md baseline updated ────────────────────────────────────────

Scenario: CLAUDE.md reflects the new PyO3 baseline
  Given the bump has been applied and committed
  When the file CLAUDE.md is read
  Then line 49 reads `pyo3 0.29` (or the actual bumped minor), not `pyo3 0.26`
  And the constraint line in the "Tech stack — Python" bullet is consistent

# ── Reject: deny.toml ignore without RISK-ACCEPTED ──────────────────────────

Scenario: deny.toml ignore entry for either advisory is rejected without approval
  Given a proposed change that adds RUSTSEC-2026-0176 or RUSTSEC-2026-0177
        to the deny.toml ignore list
  When the change is reviewed against the ADD security rule (HARD-STOP)
  Then the change is blocked — the ignore entry MUST NOT land without a signed
       RISK-ACCEPTED record (owner + expiry date) co-committed alongside it
  And if the lockstep gap forces a temporary ignore, it must carry the stopgap
      markers described in CONTRACT §3 and be approved by a human before merge

# ── Reject: partial bump leaving async-runtimes or pythonize behind ──────────

Scenario: partial bump causes mixed-pyo3-version compile failure
  Given workspace Cargo.toml bumps pyo3 to 0.29 but leaves
        pyo3-async-runtimes or pythonize at 0.26 or 0.28
  When `cargo build -p lunaris-py` is run
  Then the build fails with a "conflicting versions of pyo3" linker or type error
  And this outcome confirms the reject — the partial state MUST NOT be committed

# ── Reject: abi3-py311 / extension-module feature set silently changed ────────

Scenario: pyo3 feature flags preserved across bump
  Given the workspace pyo3 dependency line after bump
  When `grep 'pyo3.*features' Cargo.toml` is run
  Then the features string still includes both "extension-module" and "abi3-py311"
  And the wheel built by `maturin build --release` loads successfully from
      Python 3.11, 3.12, and 3.13 without a "PyO3 was compiled against a
      different version of Python" error
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
CARGO DEPENDENCY BUMP  workspace Cargo.toml + Cargo.lock
  Advisory resolution:
    RUSTSEC-2026-0176 patched = ">= 0.29.0"   (verbatim from advisory page)
    RUSTSEC-2026-0177 patched = ">= 0.29.0"   (verbatim from advisory page)
  Target version: pyo3 = "0.29"
    Justification: 0.29.0 is the SMALLEST minor that both advisories list as
    patched. No 0.27.x or 0.28.x patch release exists.

  ⚠ LOCKSTEP AVAILABILITY GAP (confirmed 2026-06-12):
    pyo3-async-runtimes latest = 0.28.0 (released 2026-02-04) — NO 0.29.x
    pythonize           latest = 0.28.0 (released 2026-02-18) — NO 0.29.x
    Both crates historically lag pyo3 minor releases by weeks.
    The default upgrade path (pyo3 0.29 + runtimes 0.29 + pythonize 0.29)
    is CURRENTLY BLOCKED. Two options at freeze:

    Option A — Wait for 0.29.x lockstep (preferred)
      Pin: pyo3 = "0.29", pyo3-async-runtimes = "0.29", pythonize = "0.29"
      Action: monitor crates.io for pyo3-async-runtimes 0.29.0 and pythonize
              0.29.0; do not merge the version bump until all three exist.
      CI gate: cargo deny check advisories MUST be green (no ignore entries).

    Option B — Temporary deny.toml stopgap (HARD-STOP without approval)
      If Option A stalls beyond an operator-defined deadline, a human must
      sign a RISK-ACCEPTED record (owner + expiry ≤ 90 days) before adding
      both advisory IDs to deny.toml ignore with upstream-blocker citations.
      The bump then targets pyo3 0.28 + runtimes 0.28 + pythonize 0.28 to
      move off 0.26.0, and the ignore entries are removed when 0.29.x lockstep
      becomes available. This option CANNOT be auto-passed by the AI.

Deliverables (exact files):
  1. Cargo.toml (workspace root)
       - Line 277: pyo3 version string updated ("0.26" → target minor)
       - Line 278: pyo3-async-runtimes version string updated
       - Line 279: pythonize version string updated
  2. Cargo.lock
       - Refreshed by `cargo update -p pyo3 -p pyo3-async-runtimes -p pythonize`
       - No other version changes outside the pyo3 stack
  3. CLAUDE.md (project root)
       - Line 49: "pyo3 0.26" → "pyo3 <target>" in the SDKs bullet
       - The "Tech stack — Python" constraint line (line 6 area) updated if
         it separately pins a baseline version
  4. crates/lunaris-py/src/generated.rs (if codegen regeneration required)
       - Only if `cargo run -p lunaris-codegen -- --emit py` produces a diff;
         the output must still satisfy: all #[pyclass] carry `dict`, none carry
         `unsendable` (asserted by grep in evidence protocol below)
  5. crates/lunaris-codegen/src/emit_py.rs (header comment only)
       - Line 1 "PyO3 0.26 emitter" and line 47 "PyO3 0.26 wrapper surface"
         updated to the target minor if codegen is touched
  6. deny.toml — NO CHANGES under Option A; see Option B above for stopgap

Evidence protocol:
  RED (test must fail before build):
    cargo deny check advisories
    # Must report RUSTSEC-2026-0176 and RUSTSEC-2026-0177 against pyo3 0.26.0
    # Exit non-zero. This is the current state on main as of 2026-06-12.

  GREEN (tests must pass after build):
    # 1. Advisory check clean
    cargo deny check advisories
    #    → exit 0, no RUSTSEC-2026-0176 or RUSTSEC-2026-0177 in output

    # 2. Workspace compiles (excluding cdylib crates per memory note)
    cargo build --workspace --exclude lunaris-py --exclude lunaris-ts

    # 3. lunaris-py SDK compiles and pytest passes
    cd crates/lunaris-py && maturin develop && cd ../..
    pytest lunaris-py/tests/ -v
    #    → 0 failures, 0 errors

    # 4. pyclass(dict)-without-unsendable invariant
    grep -E '#\[pyclass' crates/lunaris-py/src/generated.rs | grep -v 'dict'
    #    → no output (every #[pyclass] carries dict)
    grep 'unsendable' crates/lunaris-py/src/generated.rs
    #    → no output (unsendable never appears)

    # 5. Feature flags preserved
    grep 'pyo3.*features' Cargo.toml | grep -q 'extension-module'
    grep 'pyo3.*features' Cargo.toml | grep -q 'abi3-py311'
    #    → both exit 0

    # 6. Single pyo3 version in lock
    cargo tree -p lunaris-py | grep '^pyo3 ' | sort -u | wc -l
    #    → prints 1

Error semantics:
  - Mixed-version compile error ("conflicting versions of pyo3") → partial bump;
    all three crates in §Deliverables must be bumped atomically in one commit.
  - cargo deny RUSTSEC finding after bump → version not in patched range;
    verify target version is >= 0.29.0.
  - maturin develop build error after bump → pyo3 0.26→0.29 API churn in
    lunaris-py sources; inspect compiler output, update call sites
    (PyList::empty / PyTuple::new / pyo3_async_runtimes::tokio::future_into_py
    are the surfaces most likely to have signature changes across two minors).
  - pytest failure after successful maturin build → runtime binding mismatch;
    check that generated.rs was regenerated after the bump if codegen
    compilation surfaced warnings about deprecated pyo3 0.26 constructs.

Least-sure flag for freeze:
  [contract] pyo3-async-runtimes 0.29.x availability — confirmed absent as of
  2026-06-12. If it does not ship within the operator deadline, Option B
  (deny.toml stopgap at pyo3 0.28 lockstep) becomes the only path forward, and
  that path requires a human RISK-ACCEPTED approval before any CI ignore entry
  can land. This is the single point most likely to require a change request
  back to SPECIFY.
```

Status: DRAFT
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: <e.g. 90%>
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_<scenario>: arrange <Given> / act <When> / assert <Then> + assert <unchanged>
</test_plan>

Tests live in: `./tests/` · MUST run red (missing implementation) before Build.
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
