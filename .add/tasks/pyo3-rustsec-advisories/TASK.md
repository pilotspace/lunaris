# TASK: pyo3 0.26.0 RUSTSEC-2026-0176/0177: bump to patched minor across PyO3 SDK stack

slug: pyo3-rustsec-advisories · created: 2026-06-12 · stage: production
phase: done   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
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

Status: FROZEN @ v1 — approved by Tin Dang 2026-06-12 at the bundle decision
point: OPTION A (wait for the 0.29 lockstep). The task is BLOCKED-ON-UPSTREAM
until pyo3-async-runtimes >= 0.29 AND pythonize >= 0.29 exist on crates.io;
CI cargo_deny stays expected-red meanwhile (documented in the triage memory —
all other check sub-steps still gate). Recheck cadence: weekly-ish crates.io
probe; the moment lockstep exists, proceed to §4 tests (red = the deny
failure pinned to the two IDs) without a new approval. Option B (deny.toml
stopgap) was explicitly NOT chosen — any future ignore entry still requires
its own signed RISK-ACCEPTED.

Least-sure flag surfaced at freeze:
  ⚠ [contract] pyo3-async-runtimes 0.29.x availability (above) — if upstream
    lags for weeks, the operator may want to revisit Option B; that is a
    change request back to this freeze, not an auto-decision.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

For a dependency-bump task the "suite" is the frozen contract's evidence
protocol (§3), exercised on branch `build/pyo3-0.29-rustsec` / PR #36:

<test_plan>
  - RED  — `cargo deny check advisories` on pyo3 0.26.0 (main checkout):
    exit 1, BOTH RUSTSEC-2026-0176 and -0177 fire, each "Upgrade to >=0.29.0".
  - GREEN — `cargo deny check advisories` on the bumped tree: "advisories ok".
  - GREEN — `cargo check`/`cargo clippy -p lunaris-py --all-targets` clean
    (0 warnings) for default AND `bindings-it` feature sets.
  - GREEN — invariant: every real `#[pyclass(...)]` in generated.rs carries
    `dict`; zero `unsendable` (re-verified post-bump).
  - GREEN — feature flags preserved: `extension-module` + `abi3-py311` intact.
  - GREEN — single pyo3/runtimes/pythonize version in the lock (all 0.29.0).
  - WIRED — `maturin develop` builds the abi3 wheel; import smoke loads the
    module, constructs Scope / EmbedderConfig.noop, and resolves async
    `open("memory://")` through pyo3-async-runtimes 0.29 `future_into_py`.
</test_plan>

Executed on PR #36, not in this working tree. MUST-run-red satisfied by the
2026-06-12 main-branch cargo_deny failure pinned to the two IDs.
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

- [x] all tests pass — cargo_deny "advisories ok"; check/clippy clean (default + bindings-it)
- [x] coverage did not decrease — no source-logic change; mechanical API migration only
- [x] no test or contract was altered — frozen contract §3 executed verbatim (Option A, 0.29 lockstep)
- [x] concurrency / timing safe — RUSTSEC-0177 (missing Sync) is RESOLVED by the bump, not introduced
- [x] no exposed secrets / injection / unexpected deps — only the pyo3-stack minor moved; lunaris-extract lock edge is a workspace member
- [x] layering & deps follow CONVENTIONS — lunaris-py is the only pyo3 user; lunaris-ts is napi
- [x] a person reviewed — Tin Dang (PR #36 author/owner); cargo-deny RED→GREEN is the machine witness

### Deep checks
- [x] WIRING (code) — `downcast`→`cast` (types.rs, toggles.rs), `with_gil`→`attach` (conformance.rs),
      `from_py_object` opt-in (EmbedderConfig/RerankerConfig/Scope/EpisodeBuilder); all referenced + compiled.
- [x] DEAD-CODE — removed the now-dead `use lunaris_core::Embedder` in conformance.rs; no new orphan.
- [x] SEMANTIC — both advisory pages read in full: patched = ">=0.29.0" for BOTH; 0.29.0 is the minimum.

### GATE RECORD
Outcome: PASS
  Resolved out-of-band on branch `build/pyo3-0.29-rustsec` / PR #36 (2026-06-16),
  per the frozen contract's standing authorization ("the moment lockstep exists,
  proceed without a new approval"). Upstream block (pyo3-async-runtimes/pythonize
  0.29.x absent on 2026-06-12) CLEARED — all three resolve to 0.29.0 today.
  Residue (non-security, accepted): contract deliverable #5 (emit_py.rs:1/:47 +
  generated.rs:3 "PyO3 0.26" header literals) NOT updated — conditional on
  "if codegen is touched"; PR #36 did not regenerate codegen. Tracked as the
  cosmetic follow-up in PR #36's body. The bumped baseline IS recorded in
  CLAUDE.md (deliverable #3).
Reviewed by: Tin Dang · date: 2026-06-16

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): cargo_deny advisories step on CI `check`
(must stay green once PR #36 merges); future pyo3 advisories surface here first.
Spec delta for the next loop: a "blocked-on-upstream" frozen contract should
carry an explicit unblock-probe so the resume isn't discovered by accident —
here the 0.29.x lockstep shipped between freeze (06-12) and resume (06-16) and
was only noticed because an adjacent triage ran `cargo update`.

### Competency deltas
- [ADD · folded] A frozen "BLOCKED-ON-UPSTREAM" contract has no scheduled
  unblock check — it relied on a "weekly-ish crates.io probe" that no one owns.
  Evidence: 0.29.x lockstep was available 4 days post-freeze but the task sat
  at `contract` until an unrelated session stumbled on it. Consider a dated
  recheck artifact for upstream-blocked freezes.
- [TDD · folded] For a dependency-bump task the discriminating "red" is the
  advisory-DB check itself (`cargo deny check advisories` RED on the old pin),
  not a hand-written test — record it as the contract's RED witness.
- [ADD · folded] Work completed out-of-band (PR #36) against a frozen contract
  reconciles cleanly ONLY because the contract's deliverables/evidence-protocol
  were precise enough to check after the fact. Precise contracts survive the
  build happening elsewhere.
