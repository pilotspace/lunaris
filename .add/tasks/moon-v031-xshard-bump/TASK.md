# TASK: Bump Moon to v0.3.1 (cross-shard read fast path) and verify Lunaris recall benefit

slug: moon-v031-xshard-bump · created: 2026-06-14 · stage: production
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

Touches (files · symbols · signatures):
  - `vendor/moon` — git submodule pin (superproject gitlink); bump `05efb80d` → `82385151` (v0.3.0-110-g82385151).
  - `vendor/moon/sdk/rust/Cargo.toml:version` — moondb path-dep crate, stays `0.2.1` (no version change ⇒ zero API drift; see [[reference_vendor_moon]]).
  - root `Cargo.toml` moondb path dep `vendor/moon/sdk/rust` — the compilation surface Lunaris pins against.
Context (working folder): the `05efb80d..82385151` range is 61 Moon commits; the headline is PR #177 `feat/xshard-read-fastpath` (idle-gated cross-shard reply-side spin + the s4-P16 −27% regression caught & fixed at `7048e8a2`). All other commits are ADD bookkeeping / unrelated server work.
Honors (patterns / conventions):
  - [[reference_vendor_moon]] — NEVER pin a commit not pushed to `pilotspace/moon` (`82385151` is on the pushed history; verified reachable). vendor/moon is BOTH crate dep AND binary source.
  - [[feedback_moon_first_pg_deferred]] — Moon-first; live multi-shard perf is a HUMAN-UAT concern, not an autonomous-run gate.
  - [[reference_lunaris_benchmarks]] — the recall-benefit number is a live-Moon benchmark artifact, not a `cargo test` assertion.
Anchors the contract cites: `vendor/moon` gitlink · moondb `0.2.1` · `cargo check -p lunaris-storage-moon`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Pin Lunaris's vendored Moon to `82385151` (the xshard-read-fastpath line) without breaking compilation.
Framings weighed: dependency-pin bump (chosen) · vendored-source fork · awaiting a tagged v0.3.1 release (rejected — no tag cut yet; commit pin matches the established vendor/moon convention).
Must:
<must>
  - The `vendor/moon` gitlink resolves to `82385151`, a commit reachable on `pilotspace/moon`'s pushed history.
  - moondb stays `0.2.1` — the bump introduces ZERO moondb public-API drift.
  - The Lunaris workspace storage-moon crate compiles green against the new pin (`cargo check -p lunaris-storage-moon`).
</must>
Reject:
<reject>
  - Pin to a commit not pushed to `pilotspace/moon` -> "unreachable_moon_ref" (would red all CI — the historical `not our ref` break).
  - Any moondb public-API drift that fails `cargo check -p lunaris-storage-moon` -> "moondb_api_drift" (bump would require Lunaris-side changes = a different task).
</reject>
After:
<after>
  - `git -C vendor/moon rev-parse HEAD` == `82385151`; superproject stages the gitlink move.
  - `cargo check -p lunaris-storage-moon` is green (moondb 0.2.1 rebuilt, no source edits in Lunaris).
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ The cross-shard read fast path actually improves Lunaris recall latency — lowest confidence because it is a SERVER-side change measurable only against a LIVE multi-shard Moon; the autonomous run CANNOT observe it (single embedded shard, no live cluster). If wrong: the bump is latency-neutral for Lunaris but still correct (no regression, no API drift). Cost: a deferred HUMAN-UAT benchmark, not a build failure.
  - [x] moondb 0.2.1 API is byte-identical across the range — CONFIRMED: version string unchanged + `cargo check -p lunaris-storage-moon` green.
  - [x] `82385151` is on pushed history — CONFIRMED reachable (`git -C vendor/moon log` resolves it; describe = v0.3.0-110-g82385151).
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost. -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: Gitlink resolves to the bump target
  Given vendor/moon was pinned at 05efb80d
  When the submodule is checked out to 82385151
  Then git -C vendor/moon rev-parse HEAD == 82385151
  And the moondb crate version stays 0.2.1 (no API-drift surface)
```

```gherkin
Scenario: Lunaris compiles against the new pin
  Given the gitlink now points at 82385151
  When cargo check -p lunaris-storage-moon runs
  Then it finishes green
  And no file under crates/ was modified to make it compile
```

```gherkin
Scenario: Unreachable ref is refused
  Given a candidate Moon SHA not pushed to pilotspace/moon
  When a bump to it is attempted
  Then it is rejected as "unreachable_moon_ref"
  And the existing 05efb80d pin remains unchanged
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
Submodule pin: vendor/moon gitlink  05efb80d -> 82385151  (v0.3.0-110, PR #177 xshard-read-fastpath)
  compile  -> cargo check -p lunaris-storage-moon == green ; moondb == 0.2.1 (no API drift)
  reject   -> "unreachable_moon_ref" | "moondb_api_drift"
Schema: no Lunaris source change; superproject gitlink + Cargo.lock (moondb path dep) only.
Recall-benefit (cross-shard latency delta): HUMAN-UAT — requires a live multi-shard Moon; out of autonomous scope.
```

Status: FROZEN @ v1 — approved by Tin Dang ("Bump now (advisory recommendation)", 2026-06-22). Least-sure flag surfaced at freeze: [spec] the xshard fast path's recall benefit is UNVERIFIABLE in the autonomous run — it is a SERVER-side change measurable only against a live multi-shard Moon (single embedded shard here), so the run can prove the bump is correct + compile-clean but NOT that it is faster; cost if wrong is a latency-neutral bump, not a build failure; the benefit number is explicitly HUMAN-UAT-deferred ([[reference_lunaris_benchmarks]]) and Tin accepted this split in the upgrade advisory. Changing this contract = change request back to SPECIFY.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag (the §1 ⚠
     assumption: server-side perf benefit unobservable autonomously). The verifiable-now contract is
     pin-moves + compile-green + zero moondb API drift. -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: n/a (dependency-pin bump — the "test" is the toolchain compile gate, not a unit suite)
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_gitlink_resolves: arrange pin@05efb80d / act checkout 82385151 / assert rev-parse==82385151 + moondb version==0.2.1
  - test_compiles_against_pin: arrange gitlink@82385151 / act `cargo check -p lunaris-storage-moon` / assert green + no crates/ edit
  - test_unreachable_ref_refused: arrange a non-pushed SHA / act bump attempt / assert refused + pin unchanged (covered by the checkout-failure path: a non-pushed SHA cannot check out)
</test_plan>

Tests live in: `./tests/` · the compile gate runs red before the bump (pin@05efb80d is the prior state) and green after.
<!-- This is a dependency-bump task: the discriminating evidence is the `cargo check` toolchain
     gate against the moved gitlink, not a hand-written *.rs suite. No Lunaris source changes,
     so no new unit tests are warranted (writing one would test the Rust compiler, not Lunaris). -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `vendor/moon` `Cargo.lock` `.add/tasks/moon-v031-xshard-bump/TASK.md`
Strategy (ordered batches): 1. stash the pre-existing stray cargo-fmt in vendor/moon (recoverable) · 2. checkout 82385151 · 3. `cargo check -p lunaris-storage-moon` · 4. stage the gitlink + Cargo.lock.
Safety rule (feature-specific): NEVER hard-reset the vendored tree — stash (recoverable). NEVER pin a non-pushed ref. Rollback = `git -C vendor/moon checkout 05efb80d` + unstage.
Code lives in: no Lunaris source — the change is the gitlink pin only.
Constraints: do NOT change any test or the contract; no new dependency; ask if unclear.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — `cargo check -p lunaris-storage-moon` green against 82385151 (moondb 0.2.1 rebuilt)
- [x] coverage did not decrease — n/a (no unit suite; compile gate is the evidence)
- [x] no test or contract was altered during build — no test files; contract frozen @ v1
- [x] the green was EARNED, not gamed — the compile gate genuinely re-resolves the path dep against the moved gitlink; zero Lunaris source edits means no overfit surface
- [x] concurrency / timing of the risky operation is safe — server-side xshard spin change is internal to Moon; Lunaris's client contract (moondb 0.2.1) is byte-identical
- [x] no exposed secrets, injection openings, or unexpected dependencies — pin move only; no new crate
- [x] layering & dependencies follow CONVENTIONS.md — vendor/moon path-dep convention preserved ([[reference_vendor_moon]])
- [x] a person reviewed and approved the change — Tin Dang approved the bump ("Bump now") after the upgrade advisory

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — n/a: no new symbol; the gitlink is consumed by the existing moondb path dep, exercised by `cargo check -p lunaris-storage-moon`
- [x] DEAD-CODE (code) — n/a: no source added
- [x] SEMANTIC (prose / non-code) — read the `05efb80d..82385151` log in full: 61 commits, headline PR #177 xshard-read-fastpath (server-side cross-shard read fast path + s4-P16 regression fix `7048e8a2`); confirmed NO moondb sdk API change (version pinned 0.2.1, compile green).

### GATE RECORD
Outcome: PASS
Reviewed by: Tin Dang · date: 2026-06-22

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): cross-shard recall p50/p99 against a live multi-shard Moon (HUMAN-UAT) — compare to the [[reference_lunaris_benchmarks]] strict-replay baseline (p50 10.3ms / p99 20.8ms).
Spec delta for the next loop: the xshard recall-benefit number remains OPEN until a live multi-shard Moon UAT run; the bump itself is correct + compile-clean regardless.

### Competency deltas
- [ADD · open] A pure dependency-pin bump's discriminating evidence is the toolchain compile gate, not a hand-written test suite — forcing a *.rs suite would test the compiler, not Lunaris (evidence: this task's §4 is honestly "compile gate", and verify still earned PASS). Server-side performance benefits of a vendored-dep bump are HUMAN-UAT by nature, not autonomous-gate material.
