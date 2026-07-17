# TASK: Memory Feedback Tool

slug: memory-feedback-tool · created: 2026-07-17 · stage: production
autonomy: auto
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> Milestone engram-soul-loop task 4: explicit ± feedback with reason; flat
> DTO; roster test bump to 12. The third reinforcement writer (after
> trace_injection weak refs and the citation detector's strong refs).

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-core/src/activation.rs` — `Strength { Weak, Strong }`
  (serde snake_case), `WEIGHT_WEAK=1.0` / `WEIGHT_STRONG=3.0`,
  `ActivationRecord::apply` (`weighted += weight_for(s.strength)`),
  `boost_prior` (clamps activation at `max(0.0)` — a zero/negative
  `weighted` maps safely to 0.0 boost, VERIFIED line 167-171).
- `crates/lunaris-memory-service/src/record_decision.rs` — the module
  template: Params/Response DTO discipline (`deny_unknown_fields`,
  scope NEVER on the wire), `handle(lunaris, scope, params)`, INGEST-04
  (episode via `ScopedLunaris::ingest`/`ingest_idempotent` only).
- `crates/lunaris-memory-service/src/protocol.rs` — `MemoryRequest` enum
  (`op` tag snake_case), `scope()` (line ~126), `op_name()` (~143),
  `dispatch()` match (~194), `needs_embedder` (recall-only concern).
- `crates/lunaris-mcp/src/main.rs` — `#[tool]` methods build
  `MemoryRequest::X { scope: self.state.scope..., params }` and
  `decode_dto(self.proxy.dispatch(...))`; tool count today = 11.
- `crates/lunaris-mcp/tests/server_boot.rs` — `EXPECTED_TOOLS` const
  (line 27) + "all 11" doc comment; the REAL roster guard (spawns the
  binary; unit tests cannot catch outputSchema panics).
- `crates/lunaris/src/handle.rs::record_activation_refs` (~1511) — the
  task-2 sanctioned ledger writer (grouped RMW, one atomic_write).
- `crates/lunaris-hook/src/context.rs::excluded_context_source` (~1497)
  — suffix-kind exclusion list
  `matches!(kind, "memory_injection" | "turn_feedback" | "session_start" | "stop")`;
  a new always-excluded capture kind MUST be added here or it leaks into
  prompt injects (the task-1 codex-leak lesson).

Context: MILESTONE task 4 + "Reinforcement signals" locked decision
(explicit `memory.feedback` = strong ±). Dream pass (task 8) is the
downstream consumer of the feedback episodes.

Honors: MCP response DTOs are FLAT structs (rmcp outputSchema
root-object rule — the 89b9181 story); `#[serde(deny_unknown_fields)]`
on request DTOs; INGEST-04; scope alphabet; no lock across .await.

Anchors the contract cites: `Strength`, `ActivationRecord::apply`,
`MemoryRequest`, `dispatch`, `record_activation_refs`,
`excluded_context_source`, `EXPECTED_TOOLS`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: `memory.feedback` — explicit per-memory ± reinforcement with reason

Framings weighed: ledger-effect-plus-audit-episode (chosen — feedback
must both move recall NOW and leave a reasoned record for the dream
pass) · episode-only (rejected: "strong ±" decision requires a live
reinforcement effect) · ledger-only (rejected: dream pass loses the
reason text).

Must:
<must>
  - New `Strength::StrongNegative` in lunaris-core (serde
    "strong_negative", weight via `WEIGHT_STRONG_NEGATIVE: f64 = -3.0`);
    `ActivationRecord::apply` floors:
    `weighted = (weighted + w).max(0.0)`. Existing Weak/Strong behavior
    byte-identical.
  - New service module `crates/lunaris-memory-service/src/feedback.rs`:
    `FeedbackParams { memory_id: String, sentiment: Sentiment
    (positive|negative, snake_case), reason: String, dedupe_key:
    Option<String> }` (deny_unknown_fields) →
    `FeedbackResponse { lsn: String, was_duplicate: bool,
    activation_applied: bool }` (flat struct).
  - Handler: (1) parse memory_id as ULID; (2) write an episode
    source `lunaris:memory_feedback`, content = JSON {memory_id,
    sentiment, reason}, meta {kind:"feedback", sentiment, memory_id},
    via ingest / ingest_idempotent (INGEST-04); (3) apply ONE
    RefSignal{id, grain: Turn, strength: Strong | StrongNegative} via
    `record_activation_refs`; activation failure degrades to
    `activation_applied: false` (episode already durable — honest
    partial result, never an Err after the episode landed).
  - `MemoryRequest::Feedback { scope, params }` (op "feedback",
    op_name "feedback", needs_embedder = false) wired through the shared
    `dispatch()` — contextd gets it for free.
  - `#[tool(name = "memory.feedback")]` in lunaris-mcp; roster
    `EXPECTED_TOOLS` += "memory.feedback" (12 tools; update the "all 11"
    doc comment).
  - `excluded_context_source` gains the "memory_feedback" kind + a
    leak-regression test (feedback episodes must never prompt-inject).
</must>
Reject:
<reject>
  - malformed memory_id (not a ULID) -> ServiceError::InvalidInput
    ("invalid memory_id"), nothing written
  - unknown sentiment value or unknown DTO field -> serde reject at the
    wire (deny_unknown_fields / enum), nothing written
  - empty reason (after trim) -> InvalidInput ("reason required"),
    nothing written
  - a second `atomic_write` call site in feedback.rs -> forbidden;
    episode via ingest, ledger via record_activation_refs only
</reject>
After:
<after>
  - Positive feedback on a memory measurably raises its ledger weight
    (+3.0) and negative lowers it (−3.0, floored at 0) — visible through
    `boost_prior` on the next recall; a reasoned audit episode exists for
    the dream pass; the MCP roster lists 12 tools and boots.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Adding a Strength variant is serde-compatible everywhere the record
    round-trips — confidence high-medium (workspace-only consumers; old
    ledger records never contain the new variant so decode of existing
    data cannot break); verify with a decode-old-record test anyway.
  - [x] boost_prior is safe at weighted == 0 (ln→−inf → max(0.0) → 0.0
    boost) — verified in code (line 167-171).
  - [x] contextd needs no separate wiring (shared dispatch) — verified:
    dispatch() is the single match both surfaces call.
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: positive feedback strengthens the ledger and writes the audit episode
  Given a scope with a memory M whose ledger weight is 1.0 (one weak ref)
  When memory.feedback {memory_id: M, sentiment: positive, reason: "used verbatim"} runs
  Then the response has lsn set, activation_applied == true
  And M's ActivationRecord weighted == 4.0 with last_strength strong
  And an episode with source lunaris:memory_feedback and meta.kind == "feedback" exists

Scenario: negative feedback weakens the ledger, floored at zero
  Given a scope with a memory M whose ledger weight is 1.0
  When memory.feedback {sentiment: negative, reason: "misleading"} runs on M
  Then M's ActivationRecord weighted == 0.0 (1.0 − 3.0 floored)
  And last_strength == strong_negative and the audit episode exists

Scenario: invalid memory_id writes nothing
  Given any scope
  When memory.feedback {memory_id: "not-a-ulid", ...} runs
  Then the call fails InvalidInput
  And no episode and no activation record were written

Scenario: empty reason writes nothing
  When memory.feedback {reason: "  "} runs
  Then InvalidInput and nothing written

Scenario: activation write failure degrades honestly
  Given a storage that fails atomic_write only for activation keys
  When positive feedback runs
  Then the episode is written, the response returns Ok with
       activation_applied == false

Scenario: feedback episodes never prompt-inject
  Given a scope containing a lunaris:memory_feedback episode
  When the hook prompt-phase recall renders context
  Then no memory_feedback content appears in the rendered injection

Scenario: server boots with 12 tools
  When the real lunaris-mcp binary handshakes initialize -> tools/list
  Then all 12 tools including memory.feedback are listed

Scenario: dedupe replay returns the prior LSN
  When the same feedback with dedupe_key K runs twice (Moon/SQLite)
  Then the second response has was_duplicate == true and the ledger
       gained the signal only once
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
lunaris-core/src/activation.rs:
  enum Strength { Weak, Strong, StrongNegative }   # serde snake_case
  pub const WEIGHT_STRONG_NEGATIVE: f64 = -3.0;
  ActivationRecord::apply: weighted = (weighted + weight_for(s)).max(0.0)

lunaris-memory-service/src/feedback.rs (new):
  #[serde(deny_unknown_fields)] FeedbackParams
    { memory_id: String, sentiment: Sentiment, reason: String,
      dedupe_key: Option<String> #[serde(default)] }
  enum Sentiment { Positive, Negative }            # serde snake_case, JsonSchema
  FeedbackResponse { lsn: String, was_duplicate: bool,
                     activation_applied: bool }    # FLAT struct
  handle(lunaris, scope, params) -> Result<FeedbackResponse, ServiceError>
    order: validate (ULID + non-empty reason after trim) -> episode ingest
    (source "lunaris:memory_feedback", dedupe via ingest_idempotent when
    dedupe_key) -> record_activation_refs([RefSignal{ id, grain: Turn,
    strength: Strong|StrongNegative }]); signal SKIPPED when
    was_duplicate == true (replay must not double-count);
    activation Err -> tracing::warn + activation_applied=false

protocol.rs: MemoryRequest::Feedback { scope, params } · op "feedback"
  · dispatch -> feedback::handle · needs_embedder false

lunaris-mcp/src/main.rs: #[tool(name = "memory.feedback")] -> proxy
  dispatch, Json<FeedbackResponse>
tests/server_boot.rs: EXPECTED_TOOLS += "memory.feedback" (12)

lunaris-hook/src/context.rs: excluded_context_source kind list +=
  "memory_feedback"
```

Status: FROZEN @ v1 — approved by Tin (standing auto-mode delegation;
milestone task 4 scope locked in the 2026-07-16 interview).
Least-sure flag surfaced at freeze: [spec] the StrongNegative floor
semantics (−3.0, floor 0.0, no tombstone) — a strongly-downvoted
high-weight memory keeps ranking until enough negatives accumulate; cost
if wrong: UX gripe, corrected in the dream/verify wave (task 7's
invalidate path is the real kill switch). Second: [contract] skipping
the ledger signal on dedupe replays — chosen so replay-safety holds
end-to-end; if wrong (operator expects re-vote), trivially relaxed.

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: one discriminating test per scenario.
Plan:
<test_plan>
  - activation.rs unit tests: strong_negative_weight_and_floor (apply
    1.0 then −3.0 → 0.0; last_strength serde round-trips), old-record
    decode (record JSON without the new variant decodes fine).
  - feedback.rs service tests (memory:// engine): positive_strengthens
    (scenario 1, read ledger key back), negative_floors (scenario 2),
    invalid_memory_id_rejected + empty_reason_rejected (assert storage
    empty after), dedupe_replay_single_signal (scenario 8 on memory://).
  - activation-failure degrade: reuse the ActivationFailingStorage
    pattern (lunaris-hook context.rs tests ~2470) at the service level →
    activation_applied == false, episode present.
  - context.rs: feedback_kind_never_prompt_injects (seed a
    lunaris:memory_feedback episode; run the prompt-phase recall path;
    assert rendered context excludes it).
  - server_boot.rs: EXPECTED_TOOLS grows to 12 — the existing roster
    test IS the red (fails listing memory.feedback until the tool
    exists). Update the const in the RED commit.
</test_plan>

Tests live in: `crates/lunaris-core/src/activation.rs` ·
`crates/lunaris-memory-service/src/feedback.rs` ·
`crates/lunaris-hook/src/context.rs` ·
`crates/lunaris-mcp/tests/server_boot.rs` · MUST run red before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-core/src/activation.rs` ·
`crates/lunaris-memory-service/src/` (feedback.rs new, protocol.rs,
lib.rs) · `crates/lunaris-mcp/src/main.rs` ·
`crates/lunaris-mcp/tests/server_boot.rs` ·
`crates/lunaris-hook/src/context.rs`
Strategy: 1. core Strength · 2. service module + protocol · 3. mcp tool
+ roster · 4. hook exclusion.
Safety rule: episode via ingest only (INGEST-04); ledger via
record_activation_refs only; exhaustive-match check across the workspace
for the new Strength variant (clippy --workspace --all-targets).
Constraints: do NOT change any other test or the contract.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build
- [ ] the green was EARNED, not gamed
- [ ] concurrency / timing of the risky operation is safe
- [ ] no exposed secrets, injection openings, or unexpected dependencies
- [ ] layering & dependencies follow CONVENTIONS.md
- [ ] a person reviewed and approved the change

### Build expectations — what "correct" looks like (fill BEFORE build)
- [ ] a positive feedback call visibly moves the ledger record read back
      from storage (weighted +3.0) — confirmed by the service test
- [ ] the real binary lists 12 tools — confirmed by server_boot
- [ ] a seeded feedback episode is absent from a rendered prompt
      injection — confirmed by the hook test

### Deep checks — do not skim
- [ ] WIRING — mcp tool → dispatch → handler → ledger, all referenced
- [ ] DEAD-CODE — none introduced
- [ ] SEMANTIC — MILESTONE task 4 fully covered (±, reason, flat DTO,
      roster 12)

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
Reviewed by: <name> · date: <date>

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch: feedback call rate ± split · activation_applied=false rate ·
ledger floor hits (weighted clamped to 0).

### Spec delta

### Competency deltas
