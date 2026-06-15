# TASK: Wire FallbackExtractor+CircuitBreaker on the production extractor path (deferred Half A of io-failsafe-wiring)

slug: extractor-fallback-wiring · created: 2026-06-15 · stage: production
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

Feature: Wire FallbackExtractor+CircuitBreaker onto the production extractor (Half A of io-failsafe-wiring)
Framings weighed: **`fallback_wrap` seam in lunaris-extract** (chosen — a small pub helper next to FallbackExtractor + its ScriptedExtractor test infra; `default_extractor` calls it, so the wrap logic is behaviorally testable WITHOUT candle weights) · inline `Arc::new(FallbackExtractor::new(e, NoopExtractor, ..))` in default_extractor (rejected — only reachable behind candle+~3GB weights, untestable in CI) · per-call-site breaker in the ingest pipeline (rejected — FallbackExtractor already encapsulates the breaker; INGEST-04 single-atomic-write path unchanged)
Must:
<must>
  - NEW `pub fn fallback_wrap<P: Extractor>(primary: P, provider: &str) -> Arc<dyn Extractor>` in `lunaris-extract/src/fallback.rs` = `Arc::new(FallbackExtractor::new(primary, NoopExtractor, ProviderId::new(provider)))` (P: Extractor already carries Send+Sync+'static via the trait supertrait; the fallback is ALWAYS NoopExtractor — the floor)
  - `default_extractor()`'s `#[cfg(feature="candle")]` cache-HIT arm wraps the real extractor via `fallback_wrap(e, "gemma-3-4b-it")` instead of `Arc::new(e)`, so `FallbackExtractor`+`CircuitBreaker` are referenced on the production `open()` path (no longer test-only)
  - a TRANSIENT primary failure (is_transient==true) degrades to NoopExtractor (one empty RawExtraction per chunk — graph extraction off for that episode), NOT an ingest error; the breaker records the failure
  - the cache-MISS arm AND the `#[cfg(not(feature="candle"))]` arm still return a bare `NoopExtractor` (the floor — nothing to fall back FROM)
</must>
Reject:
<reject>
  - A TERMINAL primary failure (Extract(GrammarReject) / Validate / Storage(NotSupported)) -> propagates unchanged, NOT masked by the fallback (preserves FallbackExtractor's `is_transient` policy)
  - Wrapping the cache-miss/non-candle NoopExtractor in a FallbackExtractor -> rejected (a Noop primary has nothing to fall back from; only the real extractor is wrapped)
</reject>
After:
<after>
  - `fallback_wrap` is referenced by the production `default_extractor()` candle cache-hit arm; a behavioral test proves transient→Noop (empty batch, not Err) and terminal→propagate; `CircuitBreaker` is exercised on a production-path type (no longer test-only)
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ [contract] The production wiring (default_extractor's candle cache-HIT arm with the real `CandleGemma3_4B`) CANNOT be exercised end-to-end in CI — it needs ~3GB `gemma-3-4b-it` weights that CI does not stage. So wiring is proven by (a) a behavioral test of the `fallback_wrap` SEAM (the exact code the production arm calls) + (b) a STRUCTURAL source-guard that default_extractor's OK arm calls `fallback_wrap`. Lowest confidence because a structural guard proves the call-site string, not runtime behavior. If wrong: a logic regression INSIDE the OK arm that isn't a rename could slip. Cost: low — the wrap logic is fully behaviorally tested; only the 1-line call-site is structural. Honest built≠wired limitation ([[feedback_built_not_wired]]).
  - [x] NoopExtractor is the right fallback floor — confirmed: `default_extractor` already substitutes `NoopExtractor` on cache-miss, so a transient degrade matching that (no graph extraction for the episode, no ingest error) is consistent.
  - [x] "gemma-3-4b-it" is the right provider tag — confirmed: matches the existing `tracing::warn!` model name in default_extractor and the CLAUDE.md tech-stack name.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: transient primary failure degrades to the NoopExtractor fallback
  Given fallback_wrap(primary) where the primary returns a transient error (Storage(Backend))
  When extract() runs over a batch of chunks
  Then it returns NoopExtractor's result — one empty RawExtraction per chunk, NOT an error
  And the wrap's CircuitBreaker recorded the failure

Scenario: terminal primary failure propagates, never masked
  Given fallback_wrap(primary) where the primary returns a terminal error (Extract(GrammarReject))
  When extract() runs
  Then the terminal error propagates unchanged
  And the NoopExtractor fallback is NOT invoked

Scenario: default_extractor wires the fallback on the candle cache-hit arm
  Given the candle feature and the production default_extractor()
  When the cache-hit (Ok) arm builds the extractor
  Then it wraps the real extractor via fallback_wrap(e, "gemma-3-4b-it")
  And it no longer returns the bare `Arc::new(e) as Arc<dyn Extractor>`

Scenario: cache-miss / non-candle still returns a bare NoopExtractor (unchanged floor)
  Given a candle cache-miss (or the non-candle build)
  When default_extractor() resolves
  Then it returns a bare NoopExtractor (NOT wrapped — nothing to fall back from)
  And ingest still short-circuits on applies()==false
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
lunaris-extract/src/fallback.rs  (NEW production helper — no change to FallbackExtractor itself)
  pub fn fallback_wrap<P: Extractor>(primary: P, provider: &str) -> Arc<dyn Extractor>
    = Arc::new(FallbackExtractor::new(primary, NoopExtractor, ProviderId::new(provider)))
    // P: Extractor carries Send+Sync+'static (trait supertrait); fallback is ALWAYS NoopExtractor (the floor)

crates/lunaris/src/handle.rs::default_extractor  (no signature change; returns Arc<dyn Extractor>)
  #[cfg(feature="candle")]:
    Ok(e)  => lunaris_extract::fallback::fallback_wrap(e, "gemma-3-4b-it")   // WAS: Arc::new(e) as Arc<dyn Extractor>
    Err(_) => Arc::new(NoopExtractor) as Arc<dyn Extractor>                  // UNCHANGED (the floor)
  #[cfg(not(feature="candle"))]:
    Arc::new(NoopExtractor) as Arc<dyn Extractor>                           // UNCHANGED

Behavior/Errors:
  - transient primary error (is_transient==true: Storage(Backend) / Extract(Timeout|Backend))
    -> FallbackExtractor runs NoopExtractor -> Ok(empty RawExtractionBatch, one empty per chunk); breaker.on_failure()
  - terminal primary error (Extract(GrammarReject) / Validate / Storage(NotSupported))
    -> propagates unchanged; NoopExtractor NOT run
  - NO public API change; NO new dependency (FallbackExtractor/ProviderId in lunaris-extract; CircuitBreaker in lunaris-core)
```

Least-sure flag surfaced at freeze: [contract] the production `default_extractor` candle cache-HIT arm needs ~3GB `gemma-3-4b-it` weights absent in CI, so the end-to-end production path can't be exercised there. Wiring is proven by a behavioral test of the `fallback_wrap` SEAM (the exact code the OK arm calls) PLUS a structural source-guard that the OK arm calls `fallback_wrap`. Cost if the structural guard is too weak: a non-rename logic regression inside the OK arm could slip — LOW, because the wrap logic itself is fully behaviorally tested; only the 1-line call-site is structural. (built≠wired honest limitation — same class as why the eval gauntlet soft-skips without weights.)

Status: FROZEN @ v1 — approved by Tin Dang (2026-06-15)
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: each scenario → one test; all NEW tests run with NO candle weights (CI-friendly), reusing the existing `ScriptedExtractor` test helper in fallback.rs
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - `fallback_wrap_transient_degrades_to_noop` (lunaris-extract `fallback.rs` `#[cfg(test)] mod tests`): arrange `fallback_wrap(ScriptedExtractor[transient_err()], "t")` over 2 chunks / act extract() / assert Ok batch with `by_chunk.len()==2` all-empty (NoopExtractor ran), NOT Err.  [RED: `fallback_wrap` missing → lib test fails to compile]
  - `fallback_wrap_terminal_propagates` (same mod): arrange `fallback_wrap(ScriptedExtractor[terminal_err()], "t")` / act extract() / assert Err(Extract(GrammarReject)) AND the scripted fallback was not consumed.  [RED: missing fn]
  - `default_extractor_wraps_candle_primary_in_fallback` (lunaris-memory `tests/extractor_fallback_wiring.rs`, STRUCTURAL via `include_str!("../src/handle.rs")` — keeps the already-2452-line handle.rs from growing): assert the candle default_extractor source calls `fallback_wrap(e, "gemma-3-4b-it")` in the Ok arm AND no longer the bare `Ok(e) => Arc::new(e) as Arc<dyn Extractor>` (separate file from handle.rs → no self-match).  [RED: today the OK arm is `Arc::new(e) as Arc<dyn Extractor>`]
</test_plan>

Tests live in: `crates/lunaris-extract/src/fallback.rs` `crates/lunaris/tests/extractor_fallback_wiring.rs` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-extract/src/fallback.rs` `crates/lunaris/src/handle.rs`
Strategy (ordered batches):
  1. add `pub fn fallback_wrap<P: Extractor>(primary: P, provider: &str) -> Arc<dyn Extractor>` in fallback.rs (+ the 2 behavioral tests in its `#[cfg(test)] mod`)
  2. default_extractor candle OK arm: `Ok(e) => lunaris_extract::fallback::fallback_wrap(e, "gemma-3-4b-it")` (+ the structural wiring guard test in handle.rs)
  3. Run the §4 suite to green; `cargo clippy --all-targets` + fmt clean
Safety rule (feature-specific): only the cache-HIT arm is wrapped; the cache-miss + non-candle arms keep the bare NoopExtractor; INGEST-04 (one atomic_write) and the ingest fan-out are untouched; NoopExtractor.applies()==false short-circuit preserved
Code lives in: the two src files above
Constraints: do NOT change any test or the contract; allow-list packages only (no new deps); ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — `cargo test -p lunaris-extract --lib` → 35 passed (incl. the 2 new `fallback_wrap_*`); `cargo test -p lunaris-memory --test extractor_fallback_wiring --features candle` → 1 passed (wired candle arm compiles + structural guard green)
- [x] coverage did not decrease — +3 tests; none removed
- [x] no test or contract was altered during build — frozen §3 unchanged. During build I touched only §5-scoped src (fallback.rs added `fallback_wrap`; handle.rs OK arm); the §4 test files were written in the tests phase and NOT edited during build (learned from io-failsafe-wiring's scope-lock return).
- [x] the green was EARNED, not gamed — adversarial refute-read: the behavioral tests use a scripted transient/terminal primary; if `fallback_wrap` failed to wrap (returned the bare primary) the transient test would surface the primary's Err and `.expect()` would fail; if the fallback weren't NoopExtractor the empty-batch assert would fail. Terminal test proves the floor is not invoked (would be Ok otherwise). Structural guard pins the production call-site.
- [x] concurrency / timing of the risky operation is safe — `CircuitBreaker` is sync (never held across await); `fallback_wrap` is a pure constructor. INGEST-04 + the ingest fan-out untouched.
- [x] no exposed secrets, injection openings, or unexpected dependencies — no new deps (FallbackExtractor/ProviderId/NoopExtractor already in lunaris-extract; CircuitBreaker in lunaris-core)
- [x] layering & dependencies follow CONVENTIONS.md — `fallback_wrap` lives in lunaris-extract (home of FallbackExtractor); handle.rs calls it via the public `lunaris_extract::fallback::` path; only the candle cache-hit arm changed
- [~] a person reviewed and approved the change — contract human-approved at freeze (Tin Dang, the structural-wiring-guard flag surfaced + accepted); verify auto-resolved under `autonomy: auto` on complete evidence

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `default_extractor()` candle cache-hit arm now calls `lunaris_extract::fallback::fallback_wrap(e, "gemma-3-4b-it")` (handle.rs); `fallback_wrap` referenced by handle.rs + 2 behavioral tests. Wired arm compiles under `--features candle`; structural guard confirms the call-site; behavioral tests confirm the wrap logic. (Honest limitation: the real `CandleGemma3_4B` path needs ~3GB weights, not runnable in CI — surfaced + approved at freeze.)
- [x] DEAD-CODE (code) — `fallback_wrap` is `pub` + referenced; `cargo clippy -p lunaris-extract --all-targets -D warnings` → no issues. (clippy `-p lunaris-memory --features candle` flagged `doc_lazy_continuation` in `crates/lunaris-verify/src/candle_gemma3_27b.rs` — PRE-EXISTING, not in this diff; out of §5 scope, left untouched.)
- [x] SEMANTIC (prose / non-code) — n/a (code task)

### GATE RECORD
Outcome: PASS
Reviewed by: contract approved by Tin Dang @ freeze (2026-06-15); verify auto-resolved (autonomy: auto) · date: 2026-06-15 · evidence: 36 tests green across the two crates (behavioral seam RED→GREEN + structural wiring guard RED→GREEN), clippy clean on lunaris-extract, fmt clean, candle build compiles. Pre-existing lunaris-verify clippy warning noted, out of scope.

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): rate of `fallback.primary.transient_failure` tracing events (extractor degrading to Noop = graph extraction silently off for those episodes — a spike means the real extractor backend is unhealthy); breaker-open frequency on the gemma-3-4b-it provider.
Spec delta for the next loop: the wrap only covers the candle cache-HIT default extractor; `Lunaris::with_extractor(custom)` and `OllamaExtractor`/`CloudApiExtractor` paths are NOT auto-wrapped — if those become production defaults, extend `fallback_wrap` to their open() seams too. Also: io-failsafe is now whole (Half B Moon timeout + Half A extractor fallback) — the breaker is finally on a production path.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
- [TDD · open] When the production path is gated behind un-stageable assets (here: ~3GB candle weights), the discriminating test splits in two: a BEHAVIORAL test of the extracted seam (`fallback_wrap`, the exact code the gated arm calls) + a STRUCTURAL source-guard on the call-site. The seam must be the real production code, not a parallel test-only copy, or it's theater. Evidence: behavioral tests RED→GREEN on the seam; structural guard RED→GREEN on handle.rs. Honest limitation recorded + approved at freeze ([[feedback_built_not_wired]]).
- [ADD · open] Put structural `include_str!` guards in a `tests/` integration file (reading `../src/<file>.rs`), not in-module, when the target file is large/over the size budget — avoids growing it and sidesteps self-match. Evidence: handle.rs is 2452 lines (already over the 1500 convention); the guard lives in `crates/lunaris/tests/extractor_fallback_wiring.rs`.
- [ADD · open] Tests authored in the tests phase must NOT be edited during build (even a clippy/doc fix) or the scope-lock flags them — pre-empt lint issues when writing them. Evidence: io-failsafe-wiring's gate returned-to-build over an op_timeout.rs doc-fix; this task kept test files untouched through build and gated clean.
