# TASK: Wire FallbackExtractor+CircuitBreaker on the production extractor path (deferred Half A of io-failsafe-wiring)

slug: extractor-fallback-wiring · created: 2026-06-15 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it. -->
phase: ground   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower the
     autonomy level to `manual` or `conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

> Carved out of `io-failsafe-wiring` (the P0 split, 2026-06-15): that task shipped Half B
> (per-op Moon timeout) on its own; this is the deferred Half A — wire the already-built
> `FallbackExtractor`/`CircuitBreaker` onto the production extractor path so the existing
> primitives stop being test-only. Anchors verified against code 2026-06-15.

Touches (files · symbols · signatures):
  - `crates/lunaris/src/handle.rs:default_extractor` (1932, `#[cfg(feature="candle")]`) — returns `Arc<dyn Extractor>`: `CandleGemma3_4B` on cache hit, else `NoopExtractor`. Called at the production `open()` path (handle.rs:352). TODAY: no breaker, no fallback wrapper — the OK arm returns the bare gemma extractor.
  - `crates/lunaris-extract/src/fallback.rs:FallbackExtractor<P,F>` — `new(primary,fallback,ProviderId)` / `with_breaker(Arc<CircuitBreaker>)`; impls `Extractor`; breaker wraps PRIMARY, transient→fallback, terminal→propagate. `is_transient` (fallback.rs:169) classifies. ONLY built in test code today (fallback.rs:241-319) — this is the "built ≠ wired" gap.
  - `crates/lunaris-core/src/circuit_breaker.rs:CircuitBreaker` — sync primitive: `allow_request()/on_success()/on_failure()/state()`; default 5 failures/30s window/30s cooldown. NOT held across await (sync). Referenced only in lunaris-core + lunaris-extract today (never server/ingest/retrieve/storage-moon).
Context (working folder): no new files expected; edits to `crates/lunaris/src/handle.rs` (+ possibly a `fallback_wrap` helper in `crates/lunaris-extract/src/fallback.rs`). Tests in the touched crates.
Honors (patterns / conventions): global "design for failure" rule (circuit breaker on the primary); built-≠-wired ([[feedback_built_not_wired]]) — needs a DISCRIMINATING test that the PRODUCTION `open()`/`default_extractor()` path exercises the wrap, not just a unit test of FallbackExtractor; lock-not-across-await (CircuitBreaker is sync, safe).
Anchors the contract cites: `default_extractor` (FallbackExtractor wrap) · `FallbackExtractor` · `CircuitBreaker` · a new private `fallback_wrap<P>` helper.

<!-- Proposed shape (from the original io-failsafe §3, deferred — re-confirm at this task's contract freeze):
       fn fallback_wrap<P: Extractor + 'static>(primary: P) -> Arc<dyn Extractor>   // NEW, private, testable
         = Arc::new(FallbackExtractor::new(primary, NoopExtractor, ProviderId::new("gemma-3-4b-it")))
       default_extractor() OK arm calls fallback_wrap(gemma); cache-miss arm keeps the bare NoopExtractor
         (it IS the floor — nothing to fall back from). Transient primary failure → NoopExtractor (graph
         extraction off for that episode), NOT an ingest error; terminal failure propagates unchanged. -->

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: <name>
Framings weighed: <chosen> (chosen) · <alternative> · <alternative>
Must:
<must>
  - <required behavior>
</must>
Reject:
<reject>
  - <bad input / situation> -> "<error_code>"
</reject>
After:
<after>
  - <state that is true once it succeeds>
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ <the one assumption most likely to be wrong> — lowest confidence because <why>; if wrong: <cost>
  - [ ] <next assumption, ranked> — confirm or deny; never carry an open one forward
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: <short name>
  Given <starting situation>
  When <action>
  Then <expected result>
  And <what must remain unchanged>   # required for every rejection
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
<METHOD> <path>   body: { <fields> }
  200 -> { <success fields> }
  4xx -> { error: "<code>" | "<code>" }
Schema: <tables/fields touched, and access pattern>
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

Scope (may touch): `./src/`   <fill before the §3 freeze — every file the build may write>
Strategy (ordered batches): <1. … 2. … — the planned build order; guidance, not enforced>
Safety rule (feature-specific): <e.g. debit+credit in one atomic transaction>
Code lives in: `./src/`
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
