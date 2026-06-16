# TASK: Correct stale Mem0 claims in shipped docs (graph removed; latency re-cited)

slug: mem0-docs-reconcile · created: 2026-06-15 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it. -->
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

> Verified on `feat/memory-inspector` 2026-06-16. Scope NARROWED after grounding: the
> POSITIONING.md / why-lunaris.md "Ecosystem (shipped)" adapter claims are ALREADY accurate —
> `sdk-integrations-dx` (done) shipped `integrations/lunaris_integrations/{langgraph,crewai,letta}.py`
> + `pyproject.toml` extras `[langgraph]/[crewai]/[letta]` + 3 examples + `tests/test_docs_match_shipped.py`.
> So gap-analysis §F row 159 ("adapters not shipped") is OBSOLETE — do NOT touch those claims (a now-true,
> test-guarded claim). why-lunaris.md:79 graph row already reads "Mem0g (Platform-only)". The only stale
> shipped-doc claims left are in MIGRATING-FROM-MEM0.md + its book mirror.

Touches (files · symbols · signatures):
  - `docs/MIGRATING-FROM-MEM0.md` — :23 latency table row (`200–500 ms`/`p50 ≤ 25 ms`), :150 prose (`~300 ms … ~15 ms`), :178 graph bullet (`Mem0's graph features are opt-in beta`).
  - `docs/book/src/migrating/mem0.md` — byte-identical mirror: :26 table, :154 prose, :182 graph bullet.
  - `docs/competitive/mem0-gap-analysis.md` — the CANONICAL source (gated, Tin-confirmed). §F rows 156 (Mem0 latency imprecise → cite published p95 1.44 s), 157 (Lunaris real = 10.3 ms strict-replay, manual/not-CI-gated; Mem0 ~300 ms unsourced), §27/§57 (Mem0 OSS v3 REMOVED graph → Platform-only Mem0g, Neo4j, LLM on read, open deletion bugs).
Context (working folder): a pure docs reconciliation — no code paths. Benchmark facts from [[reference_lunaris_benchmarks]]: Lunaris strict-replay p50 10.3 ms / p99 20.8 ms (manual harness, NOT CI-gated). Mem0's only published figure is a p95 ~1.44 s "selective" number with a wide query-dependent range; the prior "~300 ms" was unsourced.
Honors (patterns / conventions): doc-claim validator = the "red test" (mirror `validate_gap_analysis.py`); shipped docs must not contradict the GATED gap-analysis; do NOT re-open already-correct, test-guarded claims (POSITIONING/why-lunaris adapters).
Anchors the contract cites: the three claim-types in MIGRATING-FROM-MEM0.md + mirror (graph-status, latency-table, latency-prose) and the gap-analysis §F rows that dictate the corrected wording.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Reconcile the remaining stale Mem0 claims in the two shipped migration docs (MIGRATING-FROM-MEM0.md + book mirror) so they match the GATED gap-analysis: Mem0 OSS v3 removed graph (Platform-only Mem0g), and recall-latency figures are re-cited to sourced numbers (Lunaris 10.3 ms strict-replay manual; Mem0 published p95 ~1.44 s) instead of the unsourced "opt-in beta" / "200–500 ms" / "~300 ms" claims. A committed doc-claim validator guards the reconciliation.
Framings weighed: **align-to-gated-gap-analysis + validator guard** (chosen — single source of truth, regression-pinned) · free-form rewrite (rejected — risks new unsourced claims) · widen to POSITIONING/why-lunaris (rejected — those are already correct + test-guarded by sdk-integrations-dx; re-touching = regression risk)
Must:
<must>
  - The graph bullet in BOTH files states Mem0 OSS v3 removed graph → Platform-only (Mem0g/Neo4j, LLM on read, open deletion bugs); Lunaris `Graph::anchored` is opt-in, off by default, no LLM on read.
  - The latency table row + the prose in BOTH files cite SOURCED figures: Lunaris 10.3 ms strict-replay (manual, not CI-gated); Mem0 published p95 ~1.44 s (selective, wide range). No bare "opt-in beta" / "200–500 ms" / unsourced "~300 ms".
  - The two files stay byte-identical in the reconciled regions (they are mirrors).
  - A committed validator (`tests/validate_mem0_docs.py`) greps the shipped docs: FAILS on any forbidden stale phrase, asserts the reconciled markers present in both files.
  - POSITIONING.md / why-lunaris.md adapter + graph claims are LEFT UNCHANGED (already correct + guarded by integrations/tests/test_docs_match_shipped.py).
</must>
Reject:
<reject>
  - A reconciled doc that still contradicts the gated gap-analysis (e.g. leaves "opt-in beta") -> "stale_claim_remains"
  - A new unsourced performance number (any latency figure without "strict-replay"/"manual" or "published"/dated context) -> "unsourced_number"
  - Editing POSITIONING.md / why-lunaris.md adapter claims (already true + test-guarded) -> "reopened_correct_claim"
  - The two mirror files drifting (reconciled region not identical) -> "mirror_drift"
</reject>
After:
<after>
  - Both files match the gated gap-analysis on graph-status + latency; no forbidden phrase remains.
  - validate_mem0_docs.py is committed and passes (was red before the edits).
  - POSITIONING/why-lunaris untouched; their guard test still green.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ [contract] The Mem0 published latency figure to cite is p95 ~1.44 s (selective, wide range) per the GATED gap-analysis §F row 156 — lowest confidence because Mem0's published numbers shift between releases and "selective" is a narrow benchmark; if the figure is later restated, the cite is updated via a fresh gap-analysis pass, not free-hand. Cost if wrong: a stale-but-sourced Mem0 number (still better than the current unsourced "200–500 ms").
  - [ ] Lunaris 10.3 ms / 20.8 ms strict-replay is the right number to publish (manual harness, NOT CI-gated) — confirmed via [[reference_lunaris_benchmarks]]; the caveat "manual, not CI-gated" MUST travel with it.
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: graph claim reconciled in both files
  Given MIGRATING-FROM-MEM0.md + book mirror said "Mem0's graph features are opt-in beta"
  When the validator runs after reconciliation
  Then neither file contains "opt-in beta" near Mem0
  And both state Mem0 OSS v3 removed graph / Platform-only (Mem0g) with Lunaris opt-in/no-LLM-on-read

Scenario: latency figures re-cited to sourced numbers
  Given the docs cited unsourced "200–500 ms" and "~300 ms … ~15 ms"
  When the validator runs after reconciliation
  Then neither file contains "200–500 ms" or the unsourced "~300 ms … ~15 ms" pair
  And both cite "10.3 ms" strict-replay (manual) and the Mem0 published "1.44" figure

Scenario: already-correct claims left untouched (regression guard)
  Given POSITIONING.md / why-lunaris.md adapter claims are true + test-guarded
  When the reconciliation is applied
  Then those files are unchanged
  And integrations/tests/test_docs_match_shipped.py still passes
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
DOC RECONCILIATION  (docs/MIGRATING-FROM-MEM0.md + docs/book/src/migrating/mem0.md — byte-identical regions)

  GRAPH BULLET   "Mem0's graph features are opt-in beta; so is Lunaris's Graph::anchored …"
    -> "Mem0 OSS v3 removed graph support — now Platform-only (Mem0g, Neo4j-backed, LLM on the
        read path, open deletion bugs). Lunaris's Graph::anchored(entity_ids, hops) is an opt-in
        operator, off by default, with no LLM on the read path. Both require the entity-resolution
        Extractor pipeline to have populated (entity, relation) triples first."

  LATENCY TABLE ROW  "200–500 ms (network hop to hosted) | p50 ≤ 25 ms / p99 ≤ 100 ms …"
    -> Mem0 cell: "p95 ~1.44 s (Mem0-published 'selective' figure; wide query-dependent range)"
       Lunaris cell: "10.3 ms p50 / 20.8 ms p99 strict-replay (manual bench, not CI-gated); budget
                      p50 ≤ 25 ms / p99 ≤ 100 ms on laptop-arm64 (M2 Pro)"

  LATENCY PROSE  "Recall p50 dropped from ~300 ms (network + LLM hop) to ~15 ms (embedded substrate)."
    -> "Recall p50 drops to ~10 ms on the embedded substrate (10.3 ms strict-replay bench, manual —
        not CI-gated); measure your own Mem0 baseline (Mem0's published figure is a p95 ~1.44 s,
        selective)."

VALIDATOR  tests/validate_mem0_docs.py  (the red test; pure grep over the two shipped files)
  FORBIDDEN (must be ABSENT in both): "opt-in beta", "200–500 ms", unsourced "~300 ms" prose pair
  REQUIRED  (must be PRESENT in both): "removed graph"/"Platform-only", "10.3 ms", "1.44",
            "strict-replay"; files identical in the reconciled regions
  UNTOUCHED: POSITIONING.md / why-lunaris.md NOT modified

OUT OF SCOPE: POSITIONING.md, why-lunaris.md (adapter+graph claims already correct + test-guarded)
```

Status: FROZEN @ v1 — approved by Tin Dang 2026-06-16 (gate-it-quick directive; docs-only deterministic reconciliation to the GATED gap-analysis; scope narrowed at ground after confirming adapters shipped). Least-sure flag surfaced at freeze: [contract] the Mem0 latency figure to cite is the published p95 ~1.44 s (selective) per gap-analysis §F — likely-shifting across Mem0 releases; if restated, update via a fresh gap-analysis pass, not free-hand (cost: a stale-but-sourced number, still better than today's unsourced "200–500 ms"). Changing this contract = change request back to SPECIFY.

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: 100% of the §1 Musts + Rejects (one validator assertion per claim-type + the untouched/identical guards).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_graph_claim_reconciled: assert neither file contains "opt-in beta"; both contain "removed graph"/"Platform-only" + Lunaris "no LLM on the read path" (Reject: stale_claim_remains)
  - test_latency_recited: assert neither file contains "200–500 ms" or the unsourced "~300 ms … ~15 ms" pair; both contain "10.3 ms" + "1.44" + "strict-replay" (Reject: unsourced_number)
  - test_mirror_identical: the reconciled regions are byte-identical across the two files (Reject: mirror_drift)
  - test_correct_claims_untouched: POSITIONING.md + why-lunaris.md still contain "Ecosystem (shipped)" + "Mem0g (Platform-only)" (Reject: reopened_correct_claim)
</test_plan>

Tests live in: `./tests/` · MUST run red (stale claims present) before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `docs/MIGRATING-FROM-MEM0.md` `docs/book/src/migrating/mem0.md` `./tests/`
Strategy (ordered batches): 1. write tests/validate_mem0_docs.py (red — stale phrases present). 2. apply the 3 corrections to MIGRATING-FROM-MEM0.md. 3. apply byte-identical corrections to the book mirror. 4. validator green; spot-check POSITIONING/why-lunaris unchanged.
Safety rule (feature-specific): align ONLY to the gated gap-analysis; never invent an unsourced number; never re-touch POSITIONING/why-lunaris.
Code lives in: `./tests/` (validator) + the two doc files.
Constraints: do NOT change any test or the contract; no new deps; ask if unclear.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — `validate_mem0_docs.py` GREEN (exit 0); was RED before the edits (forbidden phrases present). Covers all 4 §1 claim-types.
- [x] coverage did not decrease — validator asserts every §1 Must/Reject: FORBIDDEN-absent (opt-in beta / 200–500 ms / ~300 ms), REQUIRED-present (removed graph / Platform-only / no LLM on the read path / 10.3 ms / strict-replay / not CI-gated / 1.44), mirror-identical marker, + UNTOUCHED guard on POSITIONING/why-lunaris.
- [x] no test or contract was altered during build — `tests/validate_mem0_docs.py` unchanged since RED; §3 CONTRACT FROZEN @ v1 untouched. Only the two doc files were edited.
- [x] the green was EARNED, not gamed — validator greps the SHIPPED docs by content (not the task fixtures); the reconciled wording is the literal §3 contract text, sourced to the GATED gap-analysis §F + [[reference_lunaris_benchmarks]]. No vacuous assert (each phrase is a real string check); RED→GREEN observed.
- [N/A] concurrency / timing — pure docs reconciliation, no runtime/IO path.
- [x] no exposed secrets, injection openings, or unexpected dependencies — prose only; no code, no new deps.
- [x] layering & dependencies follow CONVENTIONS.md — honors the doc-claim-validator convention (mirrors `validate_gap_analysis.py`); shipped docs no longer contradict the gated gap-analysis.
- [x] a person reviewed and approved the change — AUTO-RESOLVED under `autonomy: auto`: complete evidence, SEMANTIC-only residue, no security/concurrency/architecture finding. Tin froze §3 v1 (2026-06-16) with the gate-it-quick directive.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] SEMANTIC (prose / non-code) — read BOTH mirrors in full. Confirmed: (1) graph bullet now states Mem0 OSS v3 removed graph → Platform-only (Mem0g/Neo4j, LLM on read, open deletion bugs); Lunaris `Graph::anchored` opt-in/off-by-default/no-LLM-on-read. (2) latency table row + prose cite SOURCED figures — Lunaris 10.3 ms p50 / 20.8 ms p99 strict-replay (manual, not CI-gated); Mem0 published p95 ~1.44 s (selective). (3) no forbidden phrase remains (validator-confirmed). (4) reconciled regions byte-identical across the two files (`diff -q` clean). (5) POSITIONING.md / why-lunaris.md unmodified (empty `git diff`).

### GATE RECORD
Outcome: PASS
Reviewed by: ADD auto-gate (autonomy:auto) — frozen by Tin Dang · date: 2026-06-17

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): validator in CI; any new shipped-doc Mem0 claim drift.
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence.
