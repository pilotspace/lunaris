# TASK: Citation Detector

slug: citation-detector · created: 2026-07-17 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it, or run `add.py autonomy set`. -->
phase: ground   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower the
     autonomy level to `manual` or `conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-hook/src/context.rs:993` — `capture_feedback(scope,
  session_id, injected_memory_ids, outcome)`: writes `lunaris:turn_feedback`
  via `capture_lightweight`; today emits ZERO activation signal and its
  `injected_memory_ids` input is ALWAYS `[]` in production (adapter reads a
  key the Stop payload never carries). The detector lands here (contextd
  socket path — the only path with an engine handle).
- `crates/lunaris-hook/src/context.rs:1040-1063` — `trace_injection`'s
  `handle_for_scope(scope) → handle.scoped().record_activation_refs()`
  log-and-continue pattern: the EXACT wiring the strong-ref upgrade mirrors.
- `crates/lunaris-hook/src/context.rs:420-429` — `ContextRequest::
  TurnFeedback` dispatch; wire variant at :99-105. `transcript_path` is NOT
  forwarded by the adapter today (dropped in run_feedback).
- `scripts/lunaris-codex-hook-adapter.py:325-335` — `run_feedback` builds
  the turn_feedback socket request (single sender for BOTH Claude Code and
  Codex; `--target` flag). Must start forwarding `transcript_path`.
- `crates/lunaris-core/src/activation.rs` — `RefSignal{id,grain,strength}`,
  `Grain::{Turn,ToolCall}`, `Strength::{Weak,Strong}` (task 2, this branch).
- TRANSCRIPT GROUND TRUTH (verified empirically on THIS machine,
  2026-07-17, 3 real transcripts under ~/.claude/projects/...):
  - JSONL entries: `type ∈ {assistant, user, attachment, system, ...}`.
  - Injections appear VERBATIM as `attachment.type ==
    "hook_additional_context"` with keys {content, hookEvent, hookName,
    toolUseID, type}; `content` holds the `<lunaris_memory_context
    phase=".." ...>` block; each memory line is
    `- [source=<s> score=<f> id=<26-char ULID>] <snippet>`.
  - Assistant tool calls: `type=="assistant"`,
    `.message.content[].type=="tool_use"` (has `id`, `name`).
  - Tool outcomes: `type=="user"`, `.message.content[].type=="tool_result"`
    with `tool_use_id` and **`is_error: true|false|null`** (856 false / 41
    true / 577 null in a 10k-line sample) — the structured success signal;
    `tool_response.success` / `.exit_code` exist only as ad-hoc blob keys.
  - The final assistant message = last `type=="assistant"` entry's text
    blocks.
- `crates/lunaris-hook/src/context.rs:1564-1571, 2213-2266` — test harness:
  `service_with_seeded_scope` + `insert_handle_for_test` +
  `insert_storage_for_test` (both caches MUST seed the same backing store);
  `trace_injection_emits_weak_activation_refs` is the scenario-mirror.
Context (working folder): `.add/milestones/engram-soul-loop/MILESTONE.md`
  task 3 + the 2026-07-17 tool-call-grain amendment; Explore dossier
  (session artifact) — key findings inlined above.
Honors (patterns / conventions): fail-open on the turn path (warn, never
  error); no transcript fixture exists in-repo (net-new fixture required);
  ids in the injection block are CHUNK/FACT ulids (Hit.id), same namespace
  task 2's ledger already keys by — consistent end-to-end, no episode-id
  translation.
Anchors the contract cites: `transcript::TurnTranscript` (new) ·
  `citation::grade_injections` (new) · `capture_feedback` ·
  `record_activation_refs` · `RefSignal` · `run_feedback` (adapter).

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

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
> Pre-declare the OBSERVABLE outcomes a correct build must produce — derived from §2 SCENARIOS
> + §3 CONTRACT — so this gate checks the build is RIGHT, not merely that tests are green. Each
> row is evidence you can SEE, not a restatement of a test name.
- [ ] <observable outcome a correct build must produce> — confirmed by <how / where>
- [ ] <another observable outcome> — confirmed by <evidence seen>

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

### Spec delta
Forward changes for the next loop — each re-enters at Specify as the next task. One line
each, tagged `[SPEC · open|seeded|dropped]`, with evidence (e.g. `[SPEC · open] rate-limit
the retry path (evidence: prod herd spikes)`). See the `add` skill's `deltas.md`.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
