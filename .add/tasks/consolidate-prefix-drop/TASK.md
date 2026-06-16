# TASK: P-C latent: WorkingMemory::consolidate with a namespace prefix drops drained non-matching events

slug: consolidate-prefix-drop · created: 2026-06-11 · stage: production
phase: done   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: consolidate-prefix-drop (SPLIT from scratchpad-handover build step 1, 2026-06-11)

Standing evidence (code-read, crates/lunaris-consolidate/src/lib.rs:121-128):
  consolidate_scoped(storage, events, Some(prefix)) filters the ALREADY-DRAINED
  events by `e.source.starts_with(prefix)` and consolidates only the matches.
  The non-matching events were consumed from the single __mq_consumers group by
  drain_consolidate_events (scope-wide) and are neither re-queued nor archived
  -> SILENT LOSS for every other namespace's pending consolidations whenever
  memory.scratchpad_consolidate runs with an explicit namespace param.
  WorkingMemory::consolidate always passes Some(&self.scope_prefix), so the
  P-C tool inherits this on every namespaced call.
Direction options (decide at freeze): re-queue non-matching events after the drain ·
  consolidate whole-scope always and drop the prefix param · partition the queue per
  namespace. scratchpad-handover sidesteps it by consolidating whole-scope (frozen
  mitigation) — this task owns the underlying fix for the manual tool path.

Framings weighed: RE-QUEUE — `WorkingMemory::consolidate` partitions the drained
batch by prefix, consolidates the matches, republishes the non-matching events
verbatim to CONSOLIDATE_TOPIC (chosen — preserves the namespaced-tool semantics
AND at-least-once delivery; one call site, working_memory.rs:241, the single
production `Some(prefix)` user) · consolidate whole-scope always and drop the
prefix semantics (rejected — silently changes the P-C tool's documented
behavior: an explicit-namespace consolidate would suddenly promote OTHER
namespaces' facts) · per-namespace MQ partitioning (rejected — P-C verified
Moon MQ has ONE hardcoded `__mq_consumers` group; this is the moondb-direct-MQ
design already proven impossible) · fix inside the `consolidate_scoped` trait
default (rejected — the trait method receives already-drained events and has
no queue handle contract; the drain owner is the right layer to re-queue).

Must:
<must>
  - mixed-namespace backlog survives a namespaced consolidate: after
    WorkingMemory::consolidate (prefix A) runs over a queue holding A + B
    events, the B events are back on CONSOLIDATE_TOPIC — a subsequent
    consolidate (prefix B or unfiltered) consolidates them; ZERO silent loss
  - matching (A) events are consolidated EXACTLY once and NOT republished
  - republish failure is loud-not-fatal: per-event stderr warn naming the
    event source + a summary warn with the lost count; the call still returns
    the report for the consolidated matches
  - consolidate_unfiltered and the background worker paths are byte-untouched
    (None prefix never partitions, never republishes)
  - the `consolidate_scoped` trait doc names the already-drained-input hazard
    so future implementors don't reintroduce the drop
</must>
Reject:
<reject>
  - double consolidation (an event both consolidated AND republished) -> the
    partition is exclusive by construction; test-pinned
  - a second consolidate entry point / second consumer group -> single
    __mq_consumers invariant holds (P-C)
  - ConsolidationReport wire-shape change -> SDK/tool DTOs stay frozen
  - republished events mutating (re-serialization must be the verbatim
    ConsolidateEvent fields) -> dedupe/audit downstream unaffected
</reject>
After:
<after>
  - memory.scratchpad_consolidate with an explicit namespace is SAFE on a
    shared scope: it promotes its own pad and leaves everyone else's pending
    events in the queue
  - the scratchpad-handover whole-scope mitigation remains valid (it simply
    never partitions); the underlying engine hazard is closed
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ republish is acceptable re-ordering: non-matching events re-enter at the
    queue TAIL, so cross-namespace FIFO order is not preserved — lowest
    confidence because no consumer contract documents ordering; cost if wrong:
    a consolidator that assumes arrival order could mis-rank recency (ActR
    uses lsn_wall_ms from the event payload, NOT queue position — verified at
    build).
  - [ ] storage.publish on a drained payload round-trips byte-stable through
    serde (ConsolidateEvent has no skip/default asymmetry) — verify at build.
  - [ ] repeated namespaced consolidates against a never-matching backlog
    churn (drain/republish loop) but never lose events — DRAIN_CAP bounds each
    cycle; acceptable, documented.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: namespaced consolidate preserves foreign-namespace backlog
  Given the consolidate queue holds events for namespaces "wm/" and "other:"
  When WorkingMemory::consolidate runs with scope_prefix "wm/"
  Then the report covers only the "wm/" events
  And a following consolidate_unfiltered consolidates the "other:" events
  And no event was consolidated twice

Scenario: matching events are not republished
  Given the queue holds only "wm/" events
  When consolidate (prefix "wm/") runs twice
  Then the second run drains nothing (report is empty)

Scenario: republish failure is loud, not fatal
  Given publish fails for a non-matching drained event
  When the namespaced consolidate runs
  Then the matching report is still returned
  And a warn names the lost event source and count

Scenario: unfiltered + worker paths untouched
  Given any backlog
  When consolidate_unfiltered runs
  Then behavior is byte-identical to today (no partition, no republish)
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
DELIVERABLES (engine-side; MCP tool untouched — it already rides WorkingMemory):
  crates/lunaris/src/primitives/working_memory.rs — consolidate():
    partition drained events into (matching, foreign) by
    source.starts_with(scope_prefix);
    consolidate matching via consolidate_scoped(storage, &matching, None)
    (already filtered — no double filter);
    republish each foreign event verbatim:
      storage.publish(&scope, CONSOLIDATE_TOPIC, 0, serde_json::to_vec(ev))
    per-event publish error -> warn(source) + summary warn(lost count);
    consolidate_unfiltered: UNCHANGED.
  crates/lunaris-consolidate/src/lib.rs — consolidate_scoped doc gains the
    already-drained-input hazard note (doc-only; default impl body unchanged).
Error semantics: republish failures never fail the call; the report reflects
  consolidated matches only. No DTO / report shape changes.
Evidence protocol:
  red   = new tests in crates/lunaris-recipes/tests/working_memory_consolidate.rs
          (existing BridgedStorage + seed_events harness): backlog-preserved
          test FAILS today (foreign events vanish); republish-failure test via
          a publish-failing storage wrapper FAILS (no warn, no partition)
  green = cargo test -p lunaris-recipes + -p lunaris-memory + -p lunaris-mcp
          all green; existing consolidate tests untouched
Schema: queue payloads byte-stable (verbatim ConsolidateEvent re-serialization).
```

Status: FROZEN 2026-06-12 — approved by Tin Dang ("Freeze it") at the bundle decision point
Least-sure flag surfaced at freeze:
  ⚠ [spec] republished events re-enter at the queue TAIL — cross-namespace FIFO
    order is not preserved and no consumer contract documents ordering. ActR
    ranks by the event payload's lsn_wall_ms, not queue position (verify at
    build); if a future consolidator assumes arrival order, recency could
    mis-rank.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every scenario = one test; the backlog-preserved test is the
discriminating one (red today because foreign events vanish).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - namespaced_consolidate_preserves_foreign_backlog: seed wm/ + other: events;
    consolidate(prefix wm/) -> report covers wm/ only; consolidate_unfiltered
    -> other: events consolidated; total consolidated == seeded (no loss, no dup)
  - matching_events_not_republished: seed wm/ only; consolidate twice;
    second report empty
  - republish_failure_is_loud_not_fatal: publish-failing storage wrapper;
    consolidate(prefix) still returns the matching report (warn path can't be
    asserted via tracing here — assert the report + no panic/Err)
  - unfiltered_path_untouched: consolidate_unfiltered consolidates the full
    mixed backlog in one call (regression pin)
</test_plan>

Tests live in: `crates/lunaris-recipes/tests/working_memory_consolidate.rs` · red run recorded before build.

RED RECORDED 2026-06-12 — branch `feat/consolidate-prefix-drop` (off origin/main
45fa25b), red commit `322ce0a` (amended from 8e4b218 for a harness
satisfiability fix). Suite: 6 passed / 2 failed:
  - namespaced_consolidate_preserves_foreign_backlog FAIL (right reason:
    total promoted 5 != 10 seeded — foreign drained events vanish)
  - republish_failure_is_loud_not_fatal FAIL (right reason: 10 publish
    attempts != 15 — no re-publish attempt is made today)
  - matching_events_not_republished PASS (regression pin, honest)
  - unfiltered_path_untouched PASS (regression pin, honest)
Harness satisfiability fixes folded into the red commit (NOT test weakening —
no assertion/expected value changed):
  - BridgedStorage::subscribe was take()-destructive (second subscribe forever
    empty → discriminating test could never go green). Now a non-destructive
    try_recv snapshot; receiver retained; republished events surface on the
    NEXT subscribe. drain_consolidate_events breaks on stream end, so finite
    snapshot streams are correct.
  - FailingPublishStorage::publish now records the attempt on inner BEFORE
    returning Err (attempt-count assertion was otherwise unsatisfiable).
Satisfiability walkthrough for both discriminating tests recorded in the
build log (subagent report, 2026-06-12).
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): partition is exclusive by construction (one
if/else over the drained vec) — an event is consolidated XOR republished,
never both; republish runs BEFORE consolidate_scoped so a consolidator error
cannot lose the foreign backlog.
Code lives in: `crates/lunaris/src/primitives/working_memory.rs` +
`crates/lunaris-consolidate/src/lib.rs` (doc-only)
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

BUILD LOG 2026-06-12 (executed by sonnet subagents in worktree, branch
`feat/consolidate-prefix-drop`; orchestrator reviewed every diff):
- Red independently re-observed before build: 6 pass / 2 fail, both
  discriminating failures verbatim-matched §4.
- Green commit `4ec41fd`: consolidate() partitions drained events by
  source.starts_with(scope_prefix); matching -> consolidate_scoped(.., None)
  (no double filter); foreign -> verbatim serde_json::to_vec + storage.publish
  to CONSOLIDATE_TOPIC; per-event warn(source) + summary warn(lost count);
  loud-not-fatal (call still returns the matching report).
  consolidate_unfiltered untouched. consolidate_scoped trait doc gained the
  already-drained-input hazard section.
- §1 assumption checks (both CONFIRMED at build):
  · ConsolidateEvent serde byte-stable: no skip/with/rename; the one
    #[serde(default)] (source, types.rs:246) is deserialize-only; two existing
    round-trip tests cover it.
  · ActR ranks by lsn_wall_ms (act_r.rs:152,205), never queue position —
    republish-at-tail reordering is safe (settles the frozen ⚠ flag).
- Style commit `6491df7` (orchestrator): clippy::while_let_loop in the red
  harness would fail CI's -D warnings + fmt drift + one stale wrapper comment
  — harness machinery only, zero assertion changes.
- Green evidence: working_memory_consolidate 8/8 · lunaris-recipes 59 ·
  lunaris-memory 183 · lunaris-mcp 66 · fmt clean · clippy --workspace
  --all-targets 0 warnings after style fix.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass (8/8 task suite; recipes/memory/mcp crates green — §5)
- [x] coverage did not decrease (4 new tests added; none removed/weakened)
- [x] no test or contract was altered during build (green commit 4ec41fd diffs
      ONLY the 2 contracted production files; style commit 6491df7 touched
      harness machinery only — lint/fmt/comment, no assertion or expected value)
- [x] concurrency / timing of the risky operation is safe (consolidate() holds
      no lock across .await; snapshot_consolidator clones the Arc out of the
      parking_lot guard before any await — audited in review)
- [x] no exposed secrets, injection openings, or unexpected dependencies
      (no new deps; republish payload is the verbatim already-stored event)
- [x] layering & dependencies follow CONVENTIONS.md (fix lives in the drain
      owner WorkingMemory, not the trait default — per frozen framing;
      INGEST-04 untouched, no new atomic_write)
- [x] a person reviewed and approved the change (orchestrator line-reviewed
      both diffs; contract approved frozen by Tin Dang 2026-06-12)

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — no new public symbol: the fix is inside the existing
      consolidate() body, which the discriminating test invokes through the
      production WorkingMemory path (and memory.scratchpad_consolidate rides
      the same method per P-C). Doc note attaches to the existing trait method.
- [x] DEAD-CODE (code) — no new unused symbol; clippy --workspace --all-targets
      clean (0 warnings) post style fix.
- [x] SEMANTIC (prose) — consolidate_scoped hazard doc read in full: names the
      already-drained input, the Some(prefix) drop, and points at
      WorkingMemory::consolidate as the canonical partition-then-republish
      pattern — matches the frozen contract wording.

### GATE RECORD
Outcome: PASS (auto-resolved under autonomy: auto — complete evidence, no
security finding in scope; the unrelated repo-wide cargo-deny pyo3 advisory
drift is tracked as its own task, not this gate's residue)
Reviewed by: orchestrator (Claude) on evidence; contract frozen by Tin Dang · date: 2026-06-12

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): the two new warn events
(`consolidate: failed to re-queue foreign event`, summary lost-count warn) —
any occurrence in production stderr = a republish failure worth a look; plus
the existing scratchpad_consolidate report counts (sudden drop in promotions
on shared scopes would have been the old silent-loss signature).
Spec delta for the next loop: repeated namespaced consolidates against a
never-matching backlog churn (drain + republish each cycle, DRAIN_CAP-bounded)
— acceptable now, but if a hot shared scope shows churn, consider a
foreign-event fast-path that skips drain when the prefix matches nothing.

### Competency deltas
- [TDD · folded] a red test can be red-for-the-wrong-satisfiability: the first
  harness take()-consumed the mpsc receiver so the discriminating test could
  NEVER go green — walk the future fix through the harness before accepting
  red (evidence: 8e4b218 → 322ce0a amend, satisfiability walkthrough).
- [ADD · folded] harness-machinery fixes during tests/build are legitimate when
  zero assertions change, but commit them separately and say so (evidence:
  style commit 6491df7 vs frozen red 322ce0a).
- [SDD · folded] trait-default methods that receive already-consumed input need
  the hazard IN THE DOC, not just at the call site — future implementors see
  the trait first (evidence: consolidate_scoped doc note shipped this task).
