# TASK: Verify Agenda Tools

slug: verify-agenda-tools · created: 2026-07-18 · stage: production
autonomy: auto
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> Milestone engram-soul-loop task 7: `memory.verify_agenda` (list stale
> memories with diff context) + `memory.resolve` (keep | supersede |
> invalidate). Consumes the task-6 verify_agenda KV. Roster 12 -> 14.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

(Explore dossier 2026-07-18 — all file:line anchors verified.)

- **DECISION — invalidate reuses `forget`, NOT `reflect_apply`.** The
  milestone text said "via the reflect_apply tombstone path", but
  `lunaris-verify::reflect_apply::apply_reflect_invalidate`
  (reflect_apply.rs:131) is FACT-keyed (`format!("fact:{ulid}")`,
  UNSCOPED — a latent bug) and never touches `episode:`. The correct,
  already-scoped, already-episode-keyed MVCC soft-delete is what
  `memory.forget` uses: `ScopedLunaris::forget(ForgetTarget::Id(ulid))`
  (handle.rs:1288 → forget.rs:570 `forget_scoped` → `build_soft_delete_op`
  = `bt.invalidate_sys(now)` + JSON-patch, ONE atomic_write D-19, ONE
  `AuditEvent::Forget` D-22). This honors the milestone's real intent
  ("reuse an existing tombstone, don't fork") better than the literal
  name. `ForgetTarget::Id(Ulid)` = OPS-01 single soft-delete;
  `ForgetReceipt.rows_written` = soft MVCC writes. Default path (no
  `.hard()`) needs no confirmation token.
- **verify_agenda KV**: `keyspace::verify_agenda_key(scope, Ulid)` /
  `verify_agenda_prefix(scope)` (keyspace.rs:260/267,
  `lunaris:{scope}:verify_agenda:{ulid}`). `VerifyAgendaEntry`
  (handle.rs:1670, `#[derive(Debug,Clone,Serialize,Deserialize)]`):
  `{episode_id: Ulid, anchor_head, current_head: String, files:
  Vec<String>, first_seen_ms, last_seen_ms: u64, v: u32}` — doc says
  KEEP STABLE for these tools. Writer `upsert_verify_agenda`
  (handle.rs:1609). **No read/delete helper exists yet — this task adds
  them.** Scan template: `digest.rs:27-58 recent_by_source`
  (scan_range + StreamExt::next loop + skip-on-corrupt).
- **Episode hydrate for snippet**: `read_as_of(scope, episode_key,
  clock.tick())` → `serde_json::from_slice::<Episode>` → `.content`
  (primitives.rs:31 has content + metadata). Fail-open to "" if
  gone/corrupt.
- **Service module templates**: `record_decision.rs` + `forget.rs`
  (DTO discipline: `#[serde(deny_unknown_fields)]`, NO scope on params,
  `pub async fn handle(lunaris: &Lunaris, scope: &Scope, params) ->
  Result<_, ServiceError>`; `let scoped = lunaris.scoped(scope.clone())`).
  Service `forget.rs:135 build_target` already maps `episode_id:
  Option<String>` → `ForgetTarget::Id(ulid)` — the exact call to reuse.
- **protocol.rs**: `MemoryRequest` enum (protocol.rs:70, `#[serde(tag=
  "op", rename_all="snake_case")]`), `scope()` (127-141), `op()`
  (145-159), `dispatch()` (196-227) — extend all four for two ops.
- **MCP**: `record_decision` #[tool] (main.rs:248-268) — 5-line body via
  `decode_dto(self.proxy.dispatch(&self.state, req).await?)`.
  `EXPECTED_TOOLS` (server_boot.rs:27-40) currently 12; append the two.
- **Response flatness** (CLAUDE.md invariant): `Json<R>` root MUST be
  `type:"object"` — flat struct, discriminator as a `status: String`
  field, NEVER a `#[serde(tag)]` enum. `ForgetResponse {removed: u64}`
  is the flat precedent.
- **No existing generic "supersede" provenance struct** (grep: only the
  verify-worker fact arbitration + one ad-hoc `superseded_by` JSON key).
  supersede here = invalidate-old (forget) + echo the caller-supplied
  replacement id; no episode-payload re-stamp in v1.

Anchors the contract cites: `ScopedLunaris::forget`, `ForgetTarget::Id`,
`VerifyAgendaEntry`, `keyspace::verify_agenda_{key,prefix}`,
`upsert_verify_agenda`, `dispatch`, `decode_dto`, `EXPECTED_TOOLS`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: `memory.verify_agenda` (list) + `memory.resolve` (act) MCP tools

Framings weighed: reuse-forget-for-invalidate (chosen — episode-scoped
MVCC tombstone already exists) · reflect_apply (rejected: fact-keyed,
unscoped, wrong domain) · new bespoke tombstone (rejected: forks D-19).
Snippet-in-list (chosen — the harness needs memory text to judge) vs
ids-only (rejected: forces a second recall round-trip per item).

Must:
<must>
  - New engine helpers on `ScopedLunaris` (crates/lunaris/src/handle.rs):
    - `list_verify_agenda(&self) -> Result<Vec<VerifyAgendaEntry>, _>`
      — scan verify_agenda_prefix, deserialize, skip-corrupt, sorted by
      last_seen_ms DESC (freshest staleness first). SCAN_CAP 5_000
      warn-and-partial.
    - `remove_verify_agenda(&self, episode_id: Ulid) -> Result<bool, _>`
      — read_as_of to detect presence, then one `WriteOp::KvDelete`
      atomic_write; returns whether an entry existed.
  - New service module `verify_agenda.rs`:
    `VerifyAgendaParams { limit: Option<usize> }` (deny_unknown_fields,
    default cap 100) → `VerifyAgendaResponse { count: usize, items:
    Vec<AgendaItem> }` (FLAT root). `AgendaItem { episode_id: String,
    anchor_head, current_head: String, files: Vec<String>,
    first_seen_ms, last_seen_ms: u64, snippet: String }` — snippet is the
    episode content trimmed to 280 chars, "" if the episode is gone
    (fail-open; a gone episode still lists so the harness can `keep`-prune
    the stale agenda row).
  - New service module `resolve.rs`:
    `ResolveParams { episode_id: String, action: ResolveAction,
    reason: Option<String>, superseded_by: Option<String> }`
    (deny_unknown_fields). `enum ResolveAction { Keep, Supersede,
    Invalidate }` (serde snake_case, JsonSchema — a REQUEST enum, allowed).
    → `ResolveResponse { status: String, episode_id: String,
    invalidated: bool, agenda_removed: bool }` (FLAT, status ∈
    kept|invalidated|superseded|not_found).
  - resolve handler order (validate episode_id ULID first):
    - Keep: `remove_verify_agenda(id)` only; invalidated=false,
      status="kept" (or "not_found" if the agenda row was absent).
    - Invalidate: `forget(ForgetTarget::Id(id))` soft-delete, THEN
      `remove_verify_agenda(id)`; invalidated=true, status="invalidated".
    - Supersede: REQUIRES `superseded_by` (a ULID) — reject
      InvalidInput("superseded_by required") if absent/malformed; else
      same as Invalidate but status="superseded" (the replacement id is
      echoed in the response + written into the ForgetReason-free audit
      via nothing extra — v1 does NOT re-stamp the episode payload,
      recorded as a spec delta).
  - `MemoryRequest::VerifyAgenda`/`Resolve` wired through dispatch (op
    "verify_agenda"/"resolve", needs_embedder false) — contextd free.
  - Two `#[tool]` methods (`memory.verify_agenda`, `memory.resolve`);
    `EXPECTED_TOOLS` 12 -> 14; boot test stays green.
</must>
Reject:
<reject>
  - invalidate forking a new tombstone writer -> forbidden; reuse
    `ScopedLunaris::forget(ForgetTarget::Id)`
  - a `#[serde(tag)]` enum as a tool `Json<R>` response root -> forbidden
    (rmcp outputSchema abort); flat struct + `status` field
  - resolve on a non-ULID episode_id -> InvalidInput, nothing written
  - supersede without a valid `superseded_by` ULID -> InvalidInput,
    nothing written / nothing invalidated
  - hard-delete via resolve -> forbidden; soft-delete only (no `.hard()`)
  - agenda scan failure aborting the whole list -> skip-corrupt, partial
    ok; a list-read failure is a ServiceError only on the storage call
    itself, never on one bad row
</reject>
After:
<after>
  - The harness lists stale memories (text + changed files + both heads)
    and resolves each: keep prunes the agenda row; invalidate/supersede
    soft-delete the episode (MVCC valid_to closed, audit published) AND
    prune the agenda row. Roster shows 14 tools and boots. A resolved
    (invalidated) episode no longer appears in a fresh `memory.recall`.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Reusing `forget(ForgetTarget::Id)` for invalidate is the milestone's
    intent despite the doc naming reflect_apply — confidence high (the
    dossier proved reflect_apply is fact-keyed/unscoped and cannot act on
    an episode ULID; forget is the ONLY scoped episode tombstone). If
    wrong: a reviewer wants a distinct audit event kind — cheap follow-up,
    the tombstone semantics are identical.
  - [x] VerifyAgendaEntry JSON is stable (handle.rs:1660 doc pins it).
  - [x] flat-struct + status field satisfies rmcp (ForgetResponse
    precedent; server_boot is the real guard).
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: verify_agenda lists stale entries with snippet + diff context
  Given a scope with two verify_agenda entries (episodes E1, E2 present)
  When memory.verify_agenda {} runs
  Then the response count == 2 and items carry episode_id, files,
       anchor_head, current_head, and a non-empty snippet for each
  And items are ordered by last_seen_ms descending

Scenario: verify_agenda lists an entry whose episode is gone (empty snippet)
  Given a verify_agenda entry whose episode row was hard-removed
  When memory.verify_agenda {} runs
  Then the entry still appears with snippet == ""

Scenario: resolve keep prunes the agenda row, episode stays valid
  Given a verify_agenda entry for episode E
  When memory.resolve {episode_id: E, action: keep} runs
  Then status == "kept", invalidated == false, agenda_removed == true
  And E is still a live (non-tombstoned) episode

Scenario: resolve invalidate soft-deletes and prunes
  Given a verify_agenda entry for a live episode E
  When memory.resolve {episode_id: E, action: invalidate} runs
  Then status == "invalidated", invalidated == true, agenda_removed true
  And E's episode row has a closed valid_to (soft tombstone)
  And a fresh recall no longer returns E

Scenario: resolve supersede requires superseded_by
  When memory.resolve {episode_id: E, action: supersede} runs (no superseded_by)
  Then InvalidInput and nothing is invalidated or removed

Scenario: resolve supersede with replacement id
  Given a verify_agenda entry for live episode E and a valid replacement ULID R
  When memory.resolve {episode_id: E, action: supersede, superseded_by: R} runs
  Then status == "superseded", invalidated == true, agenda_removed true
  And E is soft-tombstoned

Scenario: resolve keep on an absent agenda row
  When memory.resolve {episode_id: X, action: keep} runs and X has no agenda row
  Then status == "not_found", agenda_removed == false, nothing invalidated

Scenario: resolve on a malformed episode_id
  When memory.resolve {episode_id: "not-a-ulid", action: keep} runs
  Then InvalidInput and nothing written

Scenario: server boots with 14 tools
  When the real lunaris-mcp binary handshakes initialize -> tools/list
  Then all 14 tools incl. memory.verify_agenda and memory.resolve list
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
crates/lunaris/src/handle.rs (ScopedLunaris):
  pub async fn list_verify_agenda(&self)
    -> Result<Vec<VerifyAgendaEntry>, LunarisError>
    # scan verify_agenda_prefix; skip-corrupt; SCAN_CAP 5_000
    # warn-and-partial; sort last_seen_ms DESC
  pub async fn remove_verify_agenda(&self, episode_id: Ulid)
    -> Result<bool, LunarisError>
    # read_as_of presence check -> one KvDelete atomic_write; bool=existed

crates/lunaris-memory-service/src/verify_agenda.rs (new):
  #[serde(deny_unknown_fields)] VerifyAgendaParams { limit: Option<usize> }
  AgendaItem { episode_id: String, anchor_head: String,
    current_head: String, files: Vec<String>, first_seen_ms: u64,
    last_seen_ms: u64, snippet: String }        # JsonSchema
  VerifyAgendaResponse { count: usize, items: Vec<AgendaItem> }  # FLAT
  handle: list_verify_agenda -> hydrate snippet per item
    (read_as_of episode_key, Episode.content trim 280, "" on miss)
    -> truncate to limit (default 100)

crates/lunaris-memory-service/src/resolve.rs (new):
  #[serde(deny_unknown_fields)] ResolveParams { episode_id: String,
    action: ResolveAction, reason: Option<String>,
    superseded_by: Option<String> }
  enum ResolveAction { Keep, Supersede, Invalidate }  # snake_case,JsonSchema
  ResolveResponse { status: String, episode_id: String,
    invalidated: bool, agenda_removed: bool }   # FLAT, status discriminator
  handle: parse episode_id ULID -> match action:
    Keep       -> remove_verify_agenda; status kept|not_found
    Invalidate -> forget(ForgetTarget::Id) + remove; status invalidated
    Supersede  -> require superseded_by ULID; forget + remove;
                  status superseded
    (invalidated=true only for Invalidate/Supersede)

protocol.rs: MemoryRequest::VerifyAgenda{scope,params} + Resolve{scope,
  params}; scope()/op()/dispatch() arms; needs_embedder false both.
mcp/src/main.rs: #[tool "memory.verify_agenda"] + #[tool "memory.resolve"]
  (decode_dto pattern). server_boot EXPECTED_TOOLS += both (14).
```

Status: FROZEN @ v1 — approved by Tin (standing auto-mode delegation;
milestone task 7 scope locked 2026-07-16; the reflect_apply→forget
reuse-target correction is a grounded technical decision per the
blueprint-canonical rule, surfaced below).
Least-sure flag surfaced at freeze: [contract] invalidate reuses
`ScopedLunaris::forget(ForgetTarget::Id)` instead of the doc-named
`reflect_apply` — grounded decision (reflect_apply is fact-keyed +
unscoped, cannot act on an episode ULID; forget is the only scoped
episode MVCC tombstone). Same soft-delete + audit semantics the
milestone wanted, so the exit behavior is unchanged; the only cost if a
reviewer disagrees is the audit-event kind label (`Forget` vs a new
`Resolve` kind) — a trivial follow-up. Second: [spec] supersede does NOT
re-stamp the old episode's payload with `superseded_by` in v1 (no
existing provenance field to reuse) — the linkage lives only in the
response; recorded as a spec delta.

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: one discriminating test per scenario.
Plan:
<test_plan>
  - handle.rs (memory:// engine): list_verify_agenda round-trips upserted
    entries sorted by last_seen DESC; remove_verify_agenda returns
    true-then-false (idempotent) and the key is gone after.
  - verify_agenda.rs service tests: two-entry list with snippets from
    seeded episodes; gone-episode -> "" snippet but still listed; limit
    truncation.
  - resolve.rs service tests: keep prunes + episode stays live (assert
    via a follow-up recall or read_as_of not tombstoned); invalidate
    soft-tombstones (read the episode row, assert bt.sys closed) + agenda
    pruned + fresh recall omits it; supersede-without-id -> InvalidInput;
    supersede-with-id -> superseded + tombstoned; keep-on-absent ->
    not_found; malformed id -> InvalidInput.
  - server_boot.rs: EXPECTED_TOOLS 14 — the real-binary roster test is
    the red until both tools exist (update const in RED commit).
</test_plan>

Tests live in: `crates/lunaris/src/handle.rs` (or tests/) ·
`crates/lunaris-memory-service/src/verify_agenda.rs` ·
`crates/lunaris-memory-service/src/resolve.rs` ·
`crates/lunaris-mcp/tests/server_boot.rs` · MUST run red before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris/src/handle.rs` ·
`crates/lunaris-memory-service/src/` (verify_agenda.rs, resolve.rs new,
lib.rs, protocol.rs) · `crates/lunaris-mcp/src/main.rs` ·
`crates/lunaris-mcp/tests/server_boot.rs`
Strategy: 1. engine helpers (list/remove) · 2. verify_agenda service +
protocol · 3. resolve service + protocol · 4. two MCP tools + roster.
Safety rule: invalidate/supersede via `forget(ForgetTarget::Id)` soft
ONLY; one atomic_write per KV mutation; INGEST-04 not applicable (no
ingest) but no raw atomic_write outside the sanctioned engine helpers.
Constraints: no test/contract edits beyond the EXPECTED_TOOLS bump; no
new crate deps.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build (beyond EXPECTED_TOOLS)
- [ ] the green was EARNED, not gamed
- [ ] concurrency / timing of the risky operation is safe
- [ ] no exposed secrets, injection openings, or unexpected dependencies
- [ ] layering & dependencies follow CONVENTIONS.md
- [ ] a person reviewed and approved the change

### Build expectations — what "correct" looks like (fill BEFORE build)
- [ ] an invalidate call leaves the episode row with a closed valid_to
      (soft tombstone) read back in the test, AND a fresh recall omits it
- [ ] verify_agenda returns real episode snippets + the task-6 files list
- [ ] the real binary lists 14 tools

### Deep checks — do not skim
- [ ] WIRING — resolve invalidate → ScopedLunaris::forget; list →
      list_verify_agenda; both tools → dispatch
- [ ] DEAD-CODE — none
- [ ] SEMANTIC — MILESTONE task 7 covered; reflect_apply→forget decision
      documented; supersede provenance recorded as a delta

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
Reviewed by: <name> · date: <date>

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch: resolve action mix (keep/invalidate/supersede rate) · agenda size
trend post-resolve · not_found rate (agenda/episode drift).

### Spec delta
- [SPEC · open] supersede should re-stamp the old episode payload with a
  typed `superseded_by` provenance field (evidence: §1 ⚠; no field
  exists today).

### Competency deltas
