# TASK: Mem0 competitive gap analysis + ranked hardening backlog

slug: mem0-gap-analysis · created: 2026-06-14 · stage: production
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

> This is an AUDIT task: the "code it touches" is the audit surface it reads (read-only) + the one
> doc + backlog it writes. Module map lives in PROJECT.md — below is the task-delta: the exact
> anchor points each gap dimension will cite. Verified to exist via serena probes 2026-06-14.

Touches (audit surface, read-only · the dimension each anchors):
  - EXISTING competitive claims (reconcile + verify, do NOT duplicate): `docs/POSITIONING.md` · `docs/MIGRATING-FROM-MEM0.md` (+ book mirror `docs/book/src/migrating/mem0.md`) · `docs/book/src/getting-started/why-lunaris.md` · `docs/book/src/images/architecture/prompts/diagram-compare-rivals.txt` · `.planning/architect/REALITY-CHECK.md` (28 Mem0 refs — prior honest assessment)
  - Accuracy/eval: `crates/lunaris-bench/src/eval/{locomo,longmemeval,er_f1,mod}.rs` · `crates/lunaris-bench/src/bin/evals.rs` · `.github/workflows/llm-gates.yml` (does it GATE or just run?)
  - Reliability/IO fail-safe: `crates/lunaris-core/src/circuit_breaker.rs` · retry/backoff sites in `crates/lunaris-extract/src/{cloud_api,fallback}.rs` · `crates/lunaris-llm/src/cloud/mod.rs` · `crates/lunaris-verify/src/cloud_api.rs` — coverage question: which IO paths are NOT wrapped?
  - Memory-update intelligence (Mem0 ADD/UPDATE/DELETE/NOOP parity): `crates/lunaris-consolidate/src/{act_r,leiden,worker,supervisor}.rs` · `crates/lunaris/src/invalidate.rs` · `crates/lunaris/src/primitives/working_memory.rs` · ingest dedup (blake3 EntityId) `crates/lunaris-ingest/src/pipeline.rs`
  - Multi-level memory/categories/filtering: `lunaris_core::scope::Scope` · working_memory/scratchpad · retrieve DSL `crates/lunaris-retrieve/`
  - Graph quality: `crates/lunaris-storage-moon/` graph writer + `crates/lunaris-retrieve/` (FT.NAVIGATE / RRF fusion)
  - Observability/ops: `crates/lunaris-server/src/metrics.rs` (prometheus) · health/shutdown
  - SDK/DX & integrations: `crates/lunaris-{py,ts}/` · `docs/helios-integration.md` · `docs/integration/`
Context (working folder): writes `docs/competitive/mem0-gap-analysis.md` (new) + a ranked P0/P1/P2 backlog; `docs/competitive/` does not yet exist (sibling dirs: `docs/audits/`, `docs/decisions/`).
Honors (patterns / conventions): MILESTONE shared decisions — gap-analysis-first gate · built-≠-wired (every claim cites a code anchor or a fresh Mem0 source) · no stale-memory parity claims (web-verify current Mem0) · audit-not-rebuild. Public links use `github.com/pilotspace/lunaris`; benchmark numbers stay synced across the 3 canonical homes (PROJECT.md note [[project_architecture_docs_home]]).
Anchors the contract cites: the gap-analysis doc schema (one row per gap: dimension · Mem0 capability · Lunaris reality · evidence anchor · verdict · P-rank) + the P0/P1/P2 ranking rubric.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Mem0 competitive gap analysis + ranked, evidence-backed P0/P1/P2 hardening backlog
Framings weighed: **evidence-audit-and-rank** (chosen — verify every claim against code or a fresh Mem0 source, then rank by production risk) · marketing-parity-sheet (rejected — claims without evidence; the exact trap REALITY-CHECK.md exists to counter) · exhaustive-feature-matrix (rejected — unbounded, no prioritization, no production-risk lens)
Must:
<must>
  - Cover all EIGHT confirmed dimensions, each with a verdict: reliability/IO-failsafe · eval/accuracy · observability/ops · correctness/security · memory-update-intelligence · multi-level-memory+categories · graph-quality · SDK/DX-integrations
  - Every gap row carries EVIDENCE: a Lunaris code anchor (`file:symbol`) for "Lunaris reality" AND a fresh dated Mem0 source (URL + access date) for "Mem0 capability"
  - Distinguish EXISTS from WIRED-ON-PRODUCTION-PATH: a present primitive (e.g. `circuit_breaker.rs`) is verdict `partial(built-not-wired)`, never `at-parity`, unless a production call site is cited
  - Run the existing `lunaris-bench` eval gauntlet (LOCOMO/LongMemEval/ER-F1) OR record exactly why it cannot run here, and compare to Mem0's PUBLISHED numbers with a like-for-like methodology note
  - Reconcile with shipped competitive docs (`POSITIONING.md`, `MIGRATING-FROM-MEM0.md`, `REALITY-CHECK.md`): every still-true claim confirmed, every stale/false claim corrected
  - Emit a ranked P0/P1/P2 backlog; each item = `proposed_task_slug · dimension · severity · impact · acceptance_evidence · rough_effort · depends-on`, sorted by ROI (impact ÷ effort) within severity
  - Rank by the frozen anchor: P0 = production-risk OR threatens the core value contract; a Mem0 gap is P0 only if it blocks the production story, else P1 (hardening-first)
  - Each verdict uses the frozen vocabulary {ahead · at-parity · partial(built-not-wired) · gap-missing}
</must>
Reject:
<reject>
  - A Mem0-capability claim with no fresh dated source (drafted from stale memory) -> "unsourced_claim"
  - A "Lunaris reality" claim with no code anchor, or an anchor that is not on the production path, marked `at-parity` -> "unwired_claim"
  - A backlog item missing acceptance_evidence, a mapped task slug, an impact note, or a rough_effort -> "dangling_p0"
  - A latency/accuracy number quoted without a like-for-like methodology note -> "apples_to_oranges" (mirrors PROJECT.md's strict-replay rule)
  - A dimension left without a verdict -> "incomplete_coverage"
</reject>
After:
<after>
  - `docs/competitive/mem0-gap-analysis.md` exists, all 8 dimensions verdicted, every row evidence-backed, backlog ranked with mapped slugs, existing docs reconciled
  - The P0 set is presentable for human confirmation (the milestone's gap-analysis gate) and each P0 is ready to become an ADD task via loop.md
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ The P0/P1/P2 ranking RUBRIC (what counts as P0) is the highest-leverage and least-certain call — it decides what gets built first. Lowest confidence because the user selected ALL dimensions without inter-priority; if the rubric over-weights the wrong axis (e.g. DX parity over data-loss risk), the milestone builds the wrong things first → wasted waves. Mitigation: the rubric is the FREEZE-FIRST contract; human confirms the P0 set before any build task is created.
  - [ ] The `lunaris-bench` eval gauntlet is runnable in THIS environment (needs live Moon + local models + datasets per [[reference_lunaris_benchmarks]]). If not, the accuracy dimension falls back to documented historical numbers + a flagged "needs live rerun" P-item — confirm at build.
  - [ ] Mem0's published LOCOMO results are the right competitive yardstick (vs Mem0 also publishing other benchmarks). Confirm during fresh research; use whatever Mem0 leads with publicly.
  - [x] RESOLVED at freeze (Tin Dang, 2026-06-14): P0 anchor = production-risk + core value contract (hardening-first); a Mem0 gap is P0 only if it blocks production, else P1. NO hard cap on P0 — the wave is picked by ROI (impact ÷ effort).
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: All eight dimensions carry a verdict
  Given the finished docs/competitive/mem0-gap-analysis.md
  When the validator parses the gap table
  Then it finds exactly one verdict for each of the 8 dimensions
  And it exits non-zero ("incomplete_coverage") if any dimension is missing a verdict

Scenario: Every gap row is evidence-backed
  Given a row in the gap table
  When the validator checks it
  Then the Lunaris-reality column contains a path-shaped anchor that resolves to an existing file
  And the Mem0-capability column contains a URL plus an access date
  And a row missing the code anchor fails "unwired_claim" and one missing the source fails "unsourced_claim"

Scenario: built-not-wired cannot masquerade as parity
  Given a row whose verdict is "at-parity"
  When the validator checks it
  Then the row cites a production call site (not only a primitive's definition)
  And a row marked "partial(built-not-wired)" passes without a production call site
  And an "at-parity" row lacking a production anchor fails "unwired_claim"

Scenario: benchmark numbers carry methodology
  Given the accuracy/eval section quotes a latency or recall number
  When the validator checks it
  Then a like-for-like methodology note accompanies the number
  And a bare number with no methodology note fails "apples_to_oranges"

Scenario: every P0 is actionable
  Given the ranked backlog
  When the validator checks each P0 item
  Then the item has both acceptance_evidence and a proposed_task_slug
  And a P0 missing either fails "dangling_p0"

Scenario: shipped competitive docs are reconciled
  Given the reconciliation section
  When the validator checks the referenced existing docs (POSITIONING.md, MIGRATING-FROM-MEM0.md, REALITY-CHECK.md)
  Then each cited prior claim is marked "confirmed" or "corrected"
  And an existing-doc reference left unreconciled is flagged

Scenario: the doc is internally complete (green build)
  Given a fully-written gap-analysis doc that satisfies every rule above
  When the validator runs end-to-end
  Then it exits zero
  And no rule's failure code is emitted
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
ARTIFACT  docs/competitive/mem0-gap-analysis.md   (one Markdown doc; sections frozen below)
  §A Executive summary   -> per-dimension verdict line + the one-sentence headline per dimension
  §B Methodology         -> Mem0 sources (URL + access date); eval run config OR "not-run" reason; evidence standard
  §C Gap table           -> ONE row per gap; columns (frozen order):
                            | dimension | mem0_capability | lunaris_reality | evidence_anchor | mem0_source | verdict | severity |
  §D Accuracy bench      -> Lunaris eval numbers vs Mem0 published; each number + methodology note (like-for-like | flagged)
  §E Ranked backlog      -> rows: | proposed_task_slug | dimension | severity | impact | acceptance_evidence | rough_effort | depends-on |
                            sorted by ROI within severity (impact vs rough_effort); NO hard cap on P0 count
  §F Reconciliation      -> rows: | existing_doc | prior_claim | status(confirmed|corrected) | note |

  verdict ∈ { ahead · at-parity · partial(built-not-wired) · gap-missing }
  severity ∈ { P0 · P1 · P2 }
  impact   = one line: what production/competitive outcome closing this unlocks (feeds ROI ordering)
  severity RUBRIC (frozen 2026-06-14 — anchor: production-risk + core contract, hardening-first):
    P0 = risks correctness / atomicity / security / data-loss, OR threatens the CORE VALUE CONTRACT
         (sub-25ms recall · provable atomicity · opt-in graph). A Mem0 table-stakes capability we
         LACK is P0 ONLY if its absence blocks the production story; otherwise it is P1.
    P1 = competitive disadvantage with no data-risk (incl. most Mem0 parity gaps); closeable this milestone
    P2 = nice-to-have / DX polish / safely deferrable
    ordering = no fixed P0 count; the wave is chosen by ROI (impact ÷ rough_effort) at loop-time
  evidence_anchor = `path:symbol` resolving to a real file (production call site required when verdict=at-parity)

VALIDATOR  tests/validate_gap_analysis.py  (the executable gate; exit 0 = green)
  parses the doc and asserts every §1 Must; emits exactly the §1 Reject codes on failure:
    unsourced_claim · unwired_claim · dangling_p0 · apples_to_oranges · incomplete_coverage
  CLI: `python3 tests/validate_gap_analysis.py docs/competitive/mem0-gap-analysis.md`
```

Status: FROZEN @ v1 — approved by Tin Dang 2026-06-14 (P0 rubric adjusted: production-risk + core-contract anchor; ROI-ordered, no cap).
Least-sure flag surfaced at freeze: [contract] the P0 rubric axis decides build order — resolved 2026-06-14 by the production-risk + core-contract adjustment (a Mem0 gap is P0 only if it blocks production); residual [test] eval-runnability in this env is handled by the documented-fallback rule (historical numbers + a "needs live rerun" P-item if the gauntlet can't run).
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: 100% of the §1 structural rules (each Must + each Reject code has a test)
Plan (one test per scenario, asserting the validator's observable behavior — exit code + emitted reject code):
<test_plan>
  - test_incomplete_coverage: arrange doc missing one dimension's verdict / act run validator / assert exit!=0 + "incomplete_coverage"; arrange all-8 present / assert that check passes
  - test_evidence_backed: arrange row missing code anchor / assert "unwired_claim"; arrange row missing Mem0 URL+date / assert "unsourced_claim"
  - test_built_not_wired: arrange at-parity row with only a primitive definition anchor / assert "unwired_claim"; arrange partial(built-not-wired) row / assert passes
  - test_apples_to_oranges: arrange bare benchmark number / assert "apples_to_oranges"; arrange number+methodology note / assert passes
  - test_dangling_p0: arrange P0 without acceptance_evidence or slug / assert "dangling_p0"; arrange complete P0 / assert passes
  - test_reconciliation: arrange existing-doc reference with no status / assert flagged; arrange confirmed/corrected / assert passes
  - test_green_end_to_end: arrange a fully-compliant fixture doc / assert exit 0, no reject code
</test_plan>

Tests live in: `./tests/` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `docs/competitive/mem0-gap-analysis.md`   (the sole build output — a doc, not code; the validator + fixtures are written in §4 TESTS under `./tests/`)
Strategy (ordered batches):
  1. Fresh Mem0 research (web): current capabilities + published benchmark numbers + sources (URL+date) — no stale-memory claims
  2. Per-dimension Lunaris audit (fan out per Rule-5 subagents with a shared context file; verify built-vs-wired via serena + production call sites)
  3. Accuracy: run `lunaris-bench` eval gauntlet if the env allows, else record the not-run reason + historical numbers
  4. Write §A–§F; rank P0/P1/P2 by the frozen rubric; reconcile shipped competitive docs
  5. Run `tests/validate_gap_analysis.py` until green
Safety rule (feature-specific): every claim is falsifiable — code anchor or dated source; the validator is the gate, never hand-waved green
Code lives in: `docs/competitive/`
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

- [x] all tests pass — `test_validate_gap_analysis.py` GREEN (6 gate self-tests + the deliverable gate), exit 0
- [x] coverage did not decrease — new task; 100% of §1 structural rules each have a test (target met)
- [x] no test or contract was altered during build — only `docs/competitive/mem0-gap-analysis.md` written; validator + §3 unchanged since the tests->build snapshot (tripwire co-witnesses)
- [x] the green was EARNED, not gamed — the 3 highest-impact findings were re-verified directly against code, NOT trusted from subagents: eval stubs (`locomo.rs:101 let j_score = 0.0`, `er_f1.rs:90 let f1 = 0.0`); healthz stub (`healthz_handler(State(_state))` — storage handle unused, no probe); P0 built-not-wired (`FallbackExtractor::new` only in fallback.rs test bodies 241-319; `atomic.rs` has 0 `tokio::time::timeout`; `CircuitBreaker` never referenced in server/ingest/retrieve/storage-moon). No at-parity rows = no production-call-site gaming. Validator is structural, not vacuous.
- [x] concurrency / timing — N/A (deliverable is a doc + a stdlib-only Python validator; no shared state, no await)
- [x] no exposed secrets, injection openings, or unexpected dependencies — validator uses only Python stdlib (re, sys, pathlib, tempfile); doc contains public URLs only, no secrets
- [x] layering & dependencies follow CONVENTIONS.md — doc under `docs/competitive/`; honors built-≠-wired, no-stale-claims, audit-not-rebuild; public links use github.com/pilotspace/lunaris
- [~] a person reviewed and approved the change — task verify AUTO-RESOLVED under autonomy=auto on complete evidence (no security gap in the DELIVERABLE itself). The substantive human decision — confirming the P0 set + spawning hardening tasks — is escalated to the MILESTONE gap-analysis gate (presented next), not silently passed.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — the validator `validate()` + `main()` are exercised by `test_validate_gap_analysis.py` (imports + runs on fixtures and the real doc); no orphaned symbol.
- [x] DEAD-CODE (code) — none; every helper in `validate_gap_analysis.py` is called on the parse path.
- [x] SEMANTIC (prose / non-code) — read in full: the gap doc's 8 verdicts + ranked backlog. Confirmed each §C row cites a real anchor + dated Mem0 source; the contested memory-update finding is flagged in §B; the Mem0-accuracy vs Lunaris-latency comparison is explicitly marked NOT like-for-like (§D); reconciliation corrects 4 stale shipped-doc claims (Mem0g "n/a", "v0.4 adapters" shipped, two latency claims).
### Finding to escalate (not a deliverable defect): the analysis surfaced a **P0 production risk** — unbounded Moon-stall on the write path (`io-failsafe-wiring`). It is a finding ABOUT the codebase for the next wave, not a defect in this doc task; recorded in §E, escalated at the milestone gate.

### GATE RECORD
Outcome: PASS
Reviewed by: auto-resolved (autonomy=auto) on complete evidence — deliverable has no security gap; spot-verified the 3 load-bearing findings against code · date: 2026-06-14
Note: this is the TASK gate (the doc is sound + gated green). The MILESTONE gap-analysis gate — human confirmation of the P0 set before any hardening task is spawned via loop.md — is presented separately and is NOT auto-passed.

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
