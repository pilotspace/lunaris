# TASK: memory.distill — write typed knowledge record + archive sources (activation drop, provenance preserved)

slug: distill · created: 2026-07-18 · stage: production
autonomy: auto
phase: contract
<!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> engram-soul-loop **task 8b** (split from milestone task 8; 8a=dream-agenda done/merging first).
> MILESTONE.md line 110-113: "`memory.distill` writes typed records (kind ∈
> decision|lesson|invariant|gotcha, provenance ids); distilled sources archived (activation drop,
> provenance preserved); digest prefixes + source_priority extended to the new kinds." Line 44:
> keep the kind enum **extensible for a future `procedure` kind** (ATG). The harness authors the
> distilled prose (it is the judge); Lunaris is the transactional apply-tool.
> **SEQUENCING: delegate only AFTER 8a (dream-agenda) merges** — both touch protocol.rs / main.rs /
> server_boot.rs roster (14→15→16); building in parallel would conflict.

---

## 0 · GROUND — the real codebase

Touches (files · symbols · signatures):
- `crates/lunaris-core/src/activation.rs:95` — `ActivationRecord`. ADD `archived_at: Option<u64>` (`#[serde(default, skip_serializing_if = "Option::is_none")]` — old rows still decode; unarchived rows stay byte-identical, matching the KV skip_serializing convention) + `pub fn is_archived(&self) -> bool`. The struct has `#[serde(deny_unknown_fields)]` — a defaulted field is compatible. Do NOT change apply()/activation() math.
- `crates/lunaris-retrieve/src/boost_provider.rs` — `LedgerBoostProvider::priors(scope, ids)`. Records where `is_archived()` MUST contribute 0 boost (skip them). This is the "activation drop" — episode stays recall-readable (its base cosine score survives), only the usage boost is suppressed.
- `crates/lunaris/src/handle.rs` — `ScopedLunaris`. ADD `async fn archive_activation(&self, ids: &[Ulid], now: u64) -> Result<usize, LunarisError>`: RMW each `activation_key`, set `archived_at = Some(now)`; skip ids with no record (already unboosted); return count marked. Precedent: `record_activation_refs` (the ledger RMW writer) + `list_verify_agenda`.
- `crates/lunaris-memory-service/src/record_decision.rs:83` — handler shape: `source = format!("decision:{scope}")`, meta.insert("kind", …), `EpisodeBuilder::new(source, content).metadata(meta)`, `scoped.ingest` / `ingest_idempotent(builder, key)`, `IngestKind::Duplicate`. Mirror for distill.
- `crates/lunaris/src/episode_builder.rs:41,84` — `EpisodeBuilder::new`, `.id(Ulid)`, `.metadata(Map)`. Mint the distilled id so the response can return `distilled_episode_id`.
- `crates/lunaris-hook/src/context.rs:1804` — `default_digest_prefixes() -> vec!["decision:"]`. ADD `"distilled:"` so distilled knowledge feeds the SessionStart digest.
- `crates/lunaris-hook/src/context.rs:1881` — `source_priority(source)`. ADD `source.starts_with("distilled:")` → 95 (above decision:90 — distilled knowledge is the highest-value durable layer). `context.rs:1932` `dedupe_key` — ADD `distilled:` → class "distilled".
- `crates/lunaris-hook/src/context.rs:~1917` — `summarize_memory_for_context` DROPS unrecognized JSON envelopes (2026-07-14 anti-injection). **Therefore distilled `content` MUST be stored as PLAIN TEXT** (genuine plain text survives the summarizer + reaches the digest); `kind` lives in meta, NOT in a JSON content envelope. Do NOT store distill content as JSON.
- `crates/lunaris-memory-service/src/protocol.rs` — `MemoryRequest` + scope()/name()/dispatch (task-7/8a precedent). ADD `Distill`.
- `crates/lunaris-mcp/src/main.rs` — `#[tool]` precedent. `crates/lunaris-mcp/tests/server_boot.rs` — EXPECTED_TOOLS 15 (after 8a) → **16**.

Context (working folder): `.add/tasks/distill/`. Milestone lines 44, 108-113.

Honors: MCP outputSchema root type:object (FLAT response, `status` field, no `#[serde(tag)]` enum root — a request-param enum for `kind` is fine, only the RESPONSE root is constrained); deny_unknown_fields on params; server-bound scope; keyspace from lunaris_core; parking_lot, no lock across await; INGEST-04 (one atomic_write per ingest — the distilled episode is ONE ingest; ledger archive RMWs are the separate ledger write path, same as record_activation_refs); StubEmbedder for recall-based tests.

Anchors: `ActivationRecord::archived_at`/`is_archived`, `LedgerBoostProvider::priors`, `ScopedLunaris::archive_activation`, `EpisodeBuilder`, `source_priority`/`default_digest_prefixes`, `MemoryRequest::Distill`.

---

## 1 · SPECIFY — the rules

Feature: `memory.distill` — the harness sends a typed knowledge record (kind + distilled prose + provenance source ids); Lunaris writes it as a high-priority durable episode and archives the raw source episodes (activation drop, not tombstone).

Framings weighed:
- **Archive = activation drop via `archived_at` marker (chosen)** — source episode stays recall-readable (base score survives), only its usage boost is suppressed + it exits the dream-agenda candidate set; provenance preserved on the distilled record. Distinct from `forget`/`resolve` tombstone (which makes an episode unrecallable) and from the audit-only `ArchiveEvent` (which persists nothing).
- Tombstone the sources — rejected: milestone says "activation drop, provenance preserved," not "make unreadable."
- Store kind in a JSON content envelope — rejected: the hook summarizer drops unrecognized JSON (anti-injection). Plain-text content + meta.kind.

Must:
<must>
  - Validate: `kind` ∈ {decision, lesson, invariant, gotcha} (enum, extensible — reserve `procedure` in the enum comment for ATG, but do not accept it yet); `content` non-empty; `source_episode_ids` non-empty and every entry a valid ULID.
  - Write ONE durable episode: `source = format!("distilled:{kind}:{scope}")`, `content = <plain distilled text>`, meta `{ "kind": <kind>, "source_episode_ids": [<ulid strings>], "tag_count": N }`, a minted `distilled_episode_id`. Via `scoped.ingest` (or `ingest_idempotent` when `dedupe_key` present). INGEST-04: exactly one ingest.
  - Archive every source episode: `scoped.archive_activation(&source_ulids, now)` — sets `archived_at` on each existing ledger record; ids with no record are skipped (already unboosted). Return the count marked as `archived_count`.
  - The archived records contribute 0 recall boost thereafter (`LedgerBoostProvider` skips archived) AND are excluded from `memory.dream_agenda` candidates (8a already excludes archived).
  - Extend the digest/priority surface: `source_priority("distilled:…") == 95`; `default_digest_prefixes()` includes `"distilled:"`; `dedupe_key` classes `distilled:` as "distilled".
  - Idempotency: with a `dedupe_key`, a replay returns the prior `distilled_episode_id`/`lsn`, `was_duplicate=true`, and does NOT re-archive (already archived on first apply) → `archived_count=0` on the duplicate.
</must>
Reject:
<reject>
  - `source_episode_ids` empty → `"empty_provenance"`.
  - any `source_episode_ids` entry not a ULID → `"invalid_source_id"`.
  - `content` empty/whitespace → `"empty_content"`.
  - unknown `kind` (incl. `procedure` in v1) → serde reject surfaced as `"invalid_kind"`.
</reject>
After:
<after>
  - A `distilled:{kind}:{scope}` episode exists and is recall-hydratable.
  - Each source episode with a prior ledger record now has `archived_at = Some(now)`; a subsequent `memory.recall` gives those sources NO activation boost; a subsequent `memory.dream_agenda` does NOT list them as candidates.
  - The source EPISODES themselves are NOT tombstoned — still readable via `read_as_of` / recallable by base score.
  - The distilled record surfaces in the SessionStart digest (source_priority 95, prefix match) rendered as its plain text.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ **`archived_at` on `ActivationRecord` is the right archive locus** (vs a separate archive-marker key) — lowest confidence because it overloads the activation record with a lifecycle flag; chosen because it is the single row both the boost read path and the dream-agenda scan already touch, so one field suppresses both with no new keyspace. If wrong: a future need to un-archive or to archive never-referenced episodes (which have no record) forces a redesign — noted as a v2 spec-delta (archive-marker keyspace).
  - [ ] Distilled content as plain text (not JSON) renders acceptably in the digest — confirm summarize_memory_for_context passes it through (it drops JSON, keeps plain text).
  - [ ] source_priority 95 (above decision 90) is the right rank for distilled knowledge — confirm digest ordering intent.
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases

<scenarios>

```gherkin
Scenario: distill writes a typed record and archives its sources
  Given two raw episodes A,B each with an activation ledger record (referenced)
  When memory.distill is called with kind=lesson, content="prefer X over Y because Z", source_episode_ids=[A,B]
  Then a "distilled:lesson:<scope>" episode is written with meta.kind=lesson and meta.source_episode_ids=[A,B]
  And archived_count==2
  And A and B ledger records now report is_archived()==true

Scenario: archived sources lose their recall boost but stay recallable
  Given episode A archived by a prior distill
  When memory.recall surfaces A
  Then A appears (base score) but receives NO activation boost
  And A is NOT listed by memory.dream_agenda as a candidate

Scenario: distilled record ranks above decisions in the digest
  Given a "distilled:invariant:<scope>" record and a "decision:<scope>" record
  When the SessionStart digest is built with default prefixes
  Then both appear and source_priority("distilled:…")==95 > source_priority("decision:…")==90
  And the distilled record renders as its plain text (not dropped as JSON)

Scenario: idempotent replay does not re-archive
  Given a distill with dedupe_key="k" already applied (sources archived)
  When memory.distill is called again with the same dedupe_key
  Then was_duplicate==true and the prior distilled_episode_id is returned
  And archived_count==0 (no re-archive)

Scenario: reject empty provenance / bad id / empty content / unknown kind
  When memory.distill is called with source_episode_ids=[] Then error "empty_provenance"
  When called with source_episode_ids=["not-a-ulid"] Then error "invalid_source_id"
  When called with content="   " Then error "empty_content"
  When called with kind="procedure" Then error "invalid_kind"
  And in every rejection NO episode is written and NO record is archived
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape

```
TOOL memory.distill   (transactional apply; MCP + memory-service dispatch)

DistillParams  #[serde(deny_unknown_fields)]  body: {
    kind: DistillKind          // enum: decision|lesson|invariant|gotcha (serde snake_case;
                               //   `procedure` reserved in comment, rejected in v1 -> "invalid_kind")
    content: String            // plain distilled prose (NOT JSON); empty -> "empty_content"
    source_episode_ids: Vec<String>  // ULIDs; empty -> "empty_provenance"; non-ulid -> "invalid_source_id"
    title?: String
    tags?: Vec<String>
    dedupe_key?: String        // idempotency (HOOK-05)
}

200 -> DistillResponse  (FLAT struct, root type:"object") {
    status: String              // "ok" | "duplicate"
    distilled_episode_id: String
    lsn: String
    archived_count: usize
    was_duplicate: bool
}
4xx -> { error: "empty_provenance" | "invalid_source_id" | "empty_content" | "invalid_kind" }

Core (lunaris-core::activation):
    ActivationRecord += archived_at: Option<u64>  // serde(default, skip_serializing_if=Option::is_none)
    ActivationRecord::is_archived(&self) -> bool
Read (lunaris-retrieve::LedgerBoostProvider::priors): skip is_archived() records (0 boost).
Engine (lunaris::ScopedLunaris):
    async fn archive_activation(&self, ids: &[Ulid], now: u64) -> Result<usize, LunarisError>  // RMW set archived_at; skip missing; count marked
Service (lunaris-memory-service::distill::handle(lunaris, scope, params) -> Result<DistillResponse, ServiceError>)
Hook (lunaris-hook::context): source_priority "distilled:"->95; default_digest_prefixes += "distilled:"; dedupe_key class "distilled".
Dispatch: MemoryRequest::Distill { scope, params }; name/op "distill".
MCP: #[tool(name="memory.distill")]; EXPECTED_TOOLS 15 -> 16.

Access: ONE ingest (INGEST-04) for the distilled episode + N ledger RMWs to archive sources. Source episodes NOT tombstoned.
```

Status: FROZEN @ v1 — approved by Tin Dang (autonomous project-lead, engram-soul-loop standing directive)

**Lowest-confidence flag at freeze [contract]:** `archived_at` overloads `ActivationRecord` with a lifecycle flag (⚠ §1). Accepted because it's the one row both the boost read and the dream-agenda scan already read — single field, no new keyspace, suppresses both. Un-archive / archive-never-referenced deferred to a v2 archive-marker keyspace spec-delta.

---

## 4 · TESTS — failing-first suite (red)

Coverage target: 90% of the distill handler + archive_activation + archived-boost-skip branches.
Plan:
<test_plan>
  - activation.rs unit: archived_at serde round-trips; old record without the field still decodes; is_archived() true/false.
  - boost_provider unit/it: an archived record contributes 0 boost while a live one boosts (StubEmbedder recall or direct priors() call).
  - handle.rs archive_activation: marks existing records, skips missing ids, returns correct count.
  - distill service test (StubEmbedder engine): writes distilled:lesson:s episode with meta; archived_count==2; recall no-longer-boosts sources; dream_agenda no-longer-lists them.
  - digest/priority unit (lunaris-hook): source_priority("distilled:…")==95; default_digest_prefixes contains "distilled:"; a plain-text distilled record survives summarize_memory_for_context.
  - reject matrix: empty_provenance / invalid_source_id / empty_content / invalid_kind — each writes nothing + archives nothing.
  - idempotent replay: same dedupe_key -> was_duplicate, archived_count==0.
  - server_boot roster: EXPECTED_TOOLS includes memory.distill (16 total).
</test_plan>

Tests live in: `crates/lunaris-core/src/activation.rs`, `crates/lunaris-retrieve/`, `crates/lunaris/src/handle.rs`, `crates/lunaris-memory-service/src/distill.rs`, `crates/lunaris-hook/src/context.rs`, `crates/lunaris-mcp/tests/server_boot.rs`. MUST run red before Build.

---

## 5 · BUILD — AI writes code

Scope (may touch): `crates/lunaris-core/src/activation.rs` `crates/lunaris-retrieve/src/boost_provider.rs` `crates/lunaris/src/handle.rs` `crates/lunaris/src/lib.rs` `crates/lunaris-memory-service/src/distill.rs` `crates/lunaris-memory-service/src/protocol.rs` `crates/lunaris-memory-service/src/lib.rs` `crates/lunaris-hook/src/context.rs` `crates/lunaris-mcp/src/main.rs` `crates/lunaris-mcp/tests/server_boot.rs`
Strategy: 1. RED all scenarios. 2. GREEN core (archived_at + is_archived + boost skip). 3. GREEN engine (archive_activation). 4. GREEN service+MCP+dispatch. 5. GREEN hook surface (priority/digest/dedupe).
Safety rule: exactly ONE ingest for the distilled episode (INGEST-04). Archive RMWs are the ledger write path (like record_activation_refs), never a second ingest atomic_write. Source episodes NOT tombstoned. Lock never across await.
Constraints: do NOT change tests or contract; StubEmbedder for recall tests; keyspace from lunaris_core.

---

## 6 · VERIFY — evidence + non-functional review

- [ ] all tests pass; coverage held; no test/contract altered
- [ ] green EARNED — adversarial refute-read subagent; archive proven by a recall-boost-suppression assertion (not a stub)
- [ ] `archived_at` is serde-back-compat: an old ActivationRecord json (no field) decodes — test asserts it
- [ ] INGEST-04 held: exactly one ingest for the distilled episode (`grep -c atomic_write` on distill.rs == 0; the ingest goes through ScopedLunaris::ingest)
- [ ] source episodes NOT tombstoned — a read_as_of after distill still returns them
- [ ] no lock across await; layering clean

### Build expectations
- [ ] distilled:{kind}:{scope} episode written w/ meta.kind + meta.source_episode_ids — confirmed by distill service test
- [ ] archived source gets 0 recall boost yet stays recallable — confirmed by boost + recall test
- [ ] archived source drops out of memory.dream_agenda candidates — confirmed by cross-test
- [ ] MCP roster 16 incl memory.distill — server_boot.rs real-binary boot
- [ ] source_priority 95 + digest prefix + plain-text render — confirmed by hook tests

### Deep checks
- [ ] WIRING — archive_activation called by distill handler; is_archived called by boost_provider + dream candidate exclusion
- [ ] DEAD-CODE — none
- [ ] SEMANTIC — n/a

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
Reviewed by: <name> · date: <date>

---

## 7 · OBSERVE — feed the next loop

Watch: distill call rate; archived_count distribution; digest composition shift toward distilled records.

### Spec delta
- [SPEC · open] v2 archive-marker keyspace to allow archiving never-referenced episodes + un-archive (evidence: archived_at only covers episodes with a ledger record).
- [SPEC · seeded → dream-skill] task 9 `/dream` skill drives dream_agenda → distill → resolve; Stop-hook nudge computes agenda size.
- [SPEC · open] `procedure` kind for ATG procedural memory (evidence: enum reserved, rejected in v1).

### Competency deltas
- [ADD · open] archive-as-activation-drop is a new lifecycle distinct from tombstone — worth a CONVENTIONS note (evidence: three prior "archive" meanings collided: ArchiveEvent audit-only, forget/resolve tombstone, this).
