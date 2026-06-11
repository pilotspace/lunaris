# TASK: Moon hybrid path: pushed-down filter constrains BM25 branch only — dense KNN leaks through RRF

slug: moon-hybrid-filter-bypass · created: 2026-06-12 · stage: production
phase: specify   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: enforce pushed-down `Filter`s on BOTH branches of Moon's native HYBRID
search, so DSL `.filter(...)` users get correct results without per-caller
post-filtering.

SPLIT EVIDENCE (recorded verbatim from scratchpad-handover build, 2026-06-11/12):
  During the handover integration test red run, session B's fresh pad served
  session A's value: `scratchpad_read("plan")` under an empty namespace returned
  `Some("none")` (A's "blocker" value). Root cause traced:
  `crates/lunaris-retrieve/src/fusion.rs:168` —
  `compose_query_with_filter(&ctx.query.text, &ctx.query.filter)` renders the
  filter into the TEXT query passed to `moon hybrid_search` (fusion.rs:260),
  which constrains ONLY the BM25 branch. The dense-KNN branch of Moon's HYBRID
  ignores the text-query predicate, and its foreign-source hits survive RRF
  fusion. Every DSL `.filter()` user on the Moon hybrid path is affected.
  First observed 2026-06-10 ("scratchpad_read-on-Moon" note, recall-optimization
  validation); root-caused and minimally patched 2026-06-12.
  Existing per-caller mitigations (NOT the fix):
    - `lunaris-mcp::tools::recall` post-enforces `source_prefix` after recall.
    - `WorkingMemory::find` post-enforces its source filter
      (`source_filter_matches`, shipped with scratchpad-handover).

Framings weighed (decide at contract): Moon-side fix — HYBRID applies the
filter expression to the KNN candidate set (vendor/moon change; the real fix,
benefits all SDK users) · lunaris-retrieve-side post-filter inside
`fuse_via_moon_native` before RRF normalization (no Moon change; k-starvation
risk: filtered-out hits shrink the fused window) · keep per-caller guards and
document (rejected — silent wrong-results trap for every new DSL user).

Must:
<must>
  - a `.filter(...)`'d hybrid recall on Moon NEVER returns a hit violating the filter
  - fix covers Eq / StartsWith / And / Or / ValidTimeRange on indexed fields
  - k-starvation accounted for: filtering must not silently shrink top-k below
    what a filtered single-branch search would return (fan-out or re-query)
  - regression test on the live-Moon (embedded-moon or conformance) path:
    two sources, filtered hybrid query, foreign source never surfaces
</must>
Reject:
<reject>
  - per-caller post-filtering as the "fix" -> the DSL contract is the boundary
  - dropping the filter push-down entirely -> ranking quality regression
</reject>
After:
<after>
  - `WorkingMemory::find`'s `source_filter_matches` and recall.rs's source_prefix
    post-enforcement become defense-in-depth (retained), not correctness-critical
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Moon's HYBRID command can express a filter on the KNN branch at all — lowest
    confidence because the SDK signature (`hybrid_search(index, q, vec, field,
    sparse, k, weights)`) carries no filter param today; if wrong: the fix needs a
    Moon server + SDK surface change (cross-repo, vendor/moon submodule bump).
  - [ ] the BM25-branch filter rendering (compose_query_with_filter) is itself
    correct for all Filter variants — verify with conformance cases while in there.
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
