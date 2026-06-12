# TASK: FT.NAVIGATE ignores DSL filters end-to-end (Navigate operator + Moon navigate.rs)

slug: ft-navigate-filter-gap · created: 2026-06-12 · stage: production
phase: specify   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: make `.filter(...)`'d Navigate (FT.NAVIGATE) recalls on Moon honor
the filter instead of silently ignoring it.

SPLIT EVIDENCE (recorded verbatim from the moon-hybrid-filter-bypass v1.1
deep-dive, 2026-06-12, verified against moon HEAD 16bc859 + lunaris main):
  - Moon server: src/command/vector_search/navigate.rs has ZERO filter
    handling (grep 'filter|FieldFilter' -> no hits) — FT.NAVIGATE runs KNN
    seeds + graph BFS with no predicate surface.
  - Lunaris port: `StoragePort::vector_navigate` carries no filter param
    (crates/lunaris-storage-moon/src/lib.rs:254-262; raw-RESP path in
    storage-moon/src/navigate.rs).
  - Lunaris operator: operators/navigate.rs `Retriever::retrieve`
    (lines 139-145) never reads ctx.query.filter on the native path;
    the filter only reaches the capability-gated `fallback_vector`
    (lines 109-122) — and THAT filter rendering (`filter_to_moon` TAG) is
    itself dead per the moon-hybrid-filter-bypass §1 evidence.
  -> A `.filter()`'d recall using the Navigate preset returns completely
    unfiltered, graph-expanded results — same silent-wrong-results family as
    the hybrid bypass, on a different retrieval surface.

Framings weighed (decide at contract): interim guard — Navigate with
Some(filter) degrades to fallback_vector + client-side post-filter (small,
Lunaris-only, ships immediately; ranking quality loss when filtered) ·
full fix — FT.NAVIGATE gains the same FILTER clause/HybridFilter allowlist
machinery the hybrid task builds (reuses CHANGE E parser + CHANGE B
allowlist; natural follow-on AFTER moon-hybrid-filter-bypass lands; applies
the allowlist to seeds AND BFS-expanded hits) · document-only (rejected —
silent wrong results, same reason the hybrid task rejected it).
Sequencing note: this task should ride BEHIND moon-hybrid-filter-bypass —
the full fix reuses its Moon-side filter machinery verbatim.

Must:
<must>
  - a `.filter(...)`'d Navigate recall on Moon NEVER returns a hit violating
    the filter (graph-expanded hits included)
  - zero-filter Navigate behavior byte-unchanged
  - regression test: two sources, filtered Navigate recall on live Moon,
    foreign source never surfaces (incl. via BFS expansion from a matching seed)
</must>
Reject:
<reject>
  - silently ignoring the filter (today's behavior) -> the DSL contract is
    the boundary
  - filtering seeds but not BFS-expanded hits -> expansion reintroduces
    foreign sources
</reject>
After:
<after>
  - every Moon retrieval surface Lunaris exposes (vector / keyword / hybrid /
    navigate) enforces DSL filters server-side or via an explicit, documented
    degradation — no silent-ignore path remains
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ BFS-expanded hits can be filtered by the same doc_id allowlist — lowest
    confidence because expanded graph nodes may not all be text-indexed docs
    (key_to_node mapping, atomic.rs:218-221); if a BFS hop lands on a
    non-indexed node the allowlist lookup has no row to check; if wrong: the
    full fix needs per-hit HSET field reads on the expansion path (slower) or
    expansion-time pruning.
  - [ ] the Navigate preset is actually reachable with a filter in today's
    production recall presets — confirm at contract; if not reachable, the
    interim guard may be sufficient for v1 and the full fix can wait for
    demand.
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
