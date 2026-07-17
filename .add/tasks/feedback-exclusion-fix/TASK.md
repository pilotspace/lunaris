# TASK: Suffix-match excluded_context_source so legacy codex:* feedback stops leaking into prompt injections

slug: feedback-exclusion-fix · created: 2026-07-17 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it, or run `add.py autonomy set`. -->
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/`.
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-hook/src/context.rs:1328` — `fn excluded_context_source(source: &str) -> bool`:
  exact-literal `matches!` on `"lunaris:memory_injection" | "lunaris:turn_feedback" |
  "lunaris:session_start" | "lunaris:stop"`. Called from the two curation paths at
  context.rs:1213 and context.rs:1250 to keep hook-lifecycle records out of prompt injections.
Context (working folder): none — single-function fix + tests; evidence chain lives in
  `.add/milestones/engram-soul-loop/MILESTONE.md` §Grounding (verified in code 2026-07-16).
Honors (patterns / conventions): the `hook-source-prefix-lunaris` rename (2026-07-14, `dd16c88`
  era) moved live capture sources from `codex:*` to `lunaris:*` — but episodes stored BEFORE the
  rename keep their at-rest `codex:*` source strings forever. Store-side rewrites are out of
  scope (bi-temporal store; sources are immutable history).
Anchors the contract cites: `excluded_context_source` · `lunaris_excluded_sources_recognized`
  (existing unit test, context.rs:2134) · callers at context.rs:1213/1250.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: origin-agnostic exclusion of hook-lifecycle sources from context injection
Framings weighed: suffix/kind match after the origin prefix (chosen — MILESTONE.md locked
  decision) · enumerate both literal prefixes (rejected: third origin = same bug again) ·
  migrate stored codex:* sources (rejected: at-rest history is immutable, and a scan-rewrite
  is a store migration for a read-side rule)
Must:
<must>
  - the kind part of a source (text after the first `:`, or the whole string when there is
    no `:`) decides exclusion — memory_injection / turn_feedback / session_start / stop are
    excluded for ANY origin prefix (lunaris:, codex:, future ones)
  - all four current `lunaris:*` exclusions keep excluding (existing test stays green)
</must>
Reject:
<reject>
  - non-lifecycle sources must NOT become excluded: `lunaris:tool_call:post`,
    `decision:x`, `edit:y`, bare `stopwatch`-style kinds that merely share a substring ->
    they stay injectable (no error code; boolean contract)
</reject>
After:
<after>
  - a scope with pre-rename history no longer surfaces `codex:turn_feedback` /
    `codex:memory_injection` records inside prompt injections
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ kind-based matching over ALL origins is safe — lowest confidence because a future origin
    could legitimately want a "stop" kind injected; if wrong: that origin's stop records are
    silently withheld from context (cost: missing context, not corruption). Mitigated: kinds
    are hook-lifecycle names by convention (CONVENTIONS via hook capture layer).
  - [x] pre-rename data really exists in live scopes — confirmed by the 2026-07-14 deep-test
    residue + MILESTONE.md grounding note.
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: legacy codex feedback records stay out of injections
  Given an episode whose source is "codex:turn_feedback" (pre-rename at-rest data)
  When excluded_context_source(source) is evaluated on the curation path
  Then it returns true (record withheld from prompt injection)
  And lunaris:turn_feedback remains excluded exactly as before

Scenario: all four lifecycle kinds excluded for both origin prefixes
  Given sources {lunaris,codex} x {memory_injection,turn_feedback,session_start,stop}
  When excluded_context_source is evaluated for each
  Then every one returns true

Scenario: non-lifecycle sources stay injectable
  Given sources "lunaris:tool_call:post", "decision:x", "edit:y", "codex:tool_call:post"
  When excluded_context_source is evaluated for each
  Then every one returns false
  And tool-call injectability continues to be governed only by is_toolcall_capture /
      injectable_at_phase (unchanged)
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
fn excluded_context_source(source: &str) -> bool        // crates/lunaris-hook/src/context.rs
  kind := source.split_once(':').map(|(_, k)| k).unwrap_or(source)
  true  <- kind ∈ { memory_injection, turn_feedback, session_start, stop }
  false <- otherwise (incl. tool_call:* kinds, decision:*, edit:*)
Schema: none — pure read-side predicate; no storage shape touched.
```

Status: FROZEN @ v1 — approved by autonomous (autonomy=auto; decision pre-locked with Tin in
MILESTONE.md 2026-07-16: "Fix by matching the suffix, not the full literal").
Least-sure flag surfaced at freeze: [spec] kind-match applies to EVERY origin prefix, so a
future origin wanting an injectable "stop" kind would be silently withheld — why: kinds are
hook-lifecycle names by capture-layer convention; cost if wrong: missing context for that
origin, never corruption (see §1 ⚠).

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: all 3 scenarios as unit assertions in the existing context.rs test module.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - extend lunaris_excluded_sources_recognized: legacy codex:* variants of all four kinds
    assert true (RED today — exact-literal match returns false)
  - same test: non-lifecycle negatives (lunaris:tool_call:post, codex:tool_call:post,
    decision:x, edit:y) assert false (green today, pins the Reject rule)
</test_plan>

Tests live in: `crates/lunaris-hook/src/context.rs` (in-module unit tests, repo convention for
this file). MUST run red (missing implementation) before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-hook/src/context.rs`
Strategy (ordered batches): 1. red assertions in the existing unit test · 2. rewrite
  excluded_context_source to kind-match · 3. green + fmt + clippy.
Safety rule (feature-specific): exclusion may only WIDEN over lifecycle kinds; it must not
  capture tool_call/decision/edit sources (that would silently empty injections).
Constraints: do NOT change any other test or the contract; no new dependencies.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — `cargo test -p lunaris-hook --lib`: 51 passed / 0 failed
- [x] coverage did not decrease — same test count + widened assertions (8 positive, 5 negative)
- [x] no test or contract was altered during build — build touched only the predicate body;
      the test extension happened in the TESTS phase (red-proven: codex:memory_injection
      assertion failed at 2026-07-17 before the rewrite)
- [x] the green was EARNED, not gamed — assertions are input/output pairs on the public
      predicate, no shared logic with the implementation (test enumerates literals; impl
      splits on `:`); negatives pin the non-widening rule
- [x] concurrency / timing safe — pure function, no state
- [x] no exposed secrets, injection openings, or unexpected dependencies — zero new deps,
      predicate only ever WITHHOLDS records from injection
- [x] layering & dependencies follow CONVENTIONS.md — single-file change inside lunaris-hook
- [x] a person reviews at the PR gate (Tin, admin-rebase merge flow) — autonomy=auto records
      the engine gate; human review rides the PR

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
- [x] `excluded_context_source("codex:turn_feedback") == true` — confirmed by the extended
      unit test red (exact-literal) -> green (kind-match) transition
- [x] `excluded_context_source("lunaris:tool_call:post") == false` — negative assertions
      green through the rewrite (plus codex:tool_call:post / decision:x / edit:y / stopwatch)
- [x] the two curation call sites (context.rs:1213/1250) need no change — `git diff` touches
      only `excluded_context_source` + its unit test

### Deep checks — do not skim
- [x] WIRING (code) — predicate already wired at both call sites (context.rs:1213/1250);
      no new symbol introduced
- [x] DEAD-CODE (code) — no new unused symbol (rewrite-in-place)
- [x] SEMANTIC — n/a (code only)

### GATE RECORD
Outcome: PASS
Reviewed by: autonomous (autonomy=auto); human review at PR merge (Tin) · date: 2026-07-17

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): count of `*:turn_feedback` snippets appearing in injected
context on scopes with pre-rename history (expect zero after deploy).

### Spec delta
(engram-soul-loop tasks 2-9 already enumerate the follow-on work — no new delta from here yet.)

### Competency deltas
- [ADD · open] prefix-rename tasks must sweep READ-side literal matches, not only writers
  (evidence: hook-source-prefix-lunaris left this predicate exact-literal for 3 days).
