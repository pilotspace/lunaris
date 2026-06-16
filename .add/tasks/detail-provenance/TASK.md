# TASK: GET /v1/detail/{kind}/{id} — primitive detail + provenance resolved to source episodes

slug: detail-provenance · created: 2026-06-16 · stage: production
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

Single-primitive detail-by-id with provenance resolved to the SOURCE EPISODE(s). Scoped to the KV-backed kinds per Tin's call (episode/chunk/fact/community); entity/relation are graph-native (graph-endpoint task). Reuses the heterogeneous at-rest read model mapped in `browse-shape-fix` §0.

PROJECT-LEAD GROUND DECISION — the route is `GET /v1/detail/{kind}/{id}`, NOT `/v1/{kind}/{id}`: a bare `/v1/{kind}/{id}` would COLLIDE with the existing `/v1/episode/{id}` route (`lib.rs:190`) — axum 0.8 / matchit rejects an overlapping static-vs-param segment at the same position and panics at `Router` build. `/v1/detail/...` (3 segments) collides with none of `{ingest,recall,forget,snapshot/{lsn},episode/{id},scopes,browse/{kind}}`. The existing `/v1/episode/{id}` (raw episode, no provenance) stays as-is.

Touches (files · symbols · signatures):
- `crates/lunaris-server/src/routes/episode.rs:44` — the read-by-id analog: `read_as_of(&scope, &key, state.lunaris.clock().tick()) -> Result<Option<Row<Bytes>>>`; `Ok(Some(row))` → `row.value` is raw JSON; `Ok(None)` → 404; `Err` → `map_error(Storage)`. Detail generalizes this over `{kind}_key` + adds provenance resolution.
- `crates/lunaris-server/src/lib.rs:190` — the `/episode/{id}` route block to mirror (rate_limit → tracing → `scoped_auth("recall")`); NEW route `/detail/{kind}/{id}` registers the same way.
- `lunaris_core::keyspace::{episode,chunk,fact,community}_key(&Scope, Ulid)` — the per-kind read key.
- `lunaris_extract::types::Fact` (`fact:` at-rest) — carries `confidence` + `subject_id`/`object_id` (EntityId; `Display` = lowercase hex). NOTE: `source_episode_id` is written by `structured_ingest` but is NOT a field of `extract::Fact` (it's dropped on typed deser) — so the fact provenance read parses the RAW `serde_json::Value` to find `source_episode_id`, then resolves it.
- `lunaris_core::Chunk.episode_id: Ulid` (serde `"episode_id"`) — the chunk's source episode; `core::Episode`/`Community` are the at-rest episode/community shapes.
- `crates/lunaris-server/src/middleware/error.rs:24` — `map_error` envelope `{ error, message }`; reused for 404/500.
- `crates/lunaris/src/handle.rs:823/841` — `Lunaris::storage()`/`clock()`.

Context (working folder): `crates/lunaris-server/src/routes/browse.rs` (or a new `detail.rs`) for the handler + 1 route in `lib.rs`. No DTO/migration. The `lunaris-extract` dep already added by browse-shape-fix.

Honors (patterns / conventions): scope = JWT `claims.scope` only; `read_as_of` "now" via `clock().tick()`; `map_error` envelope; **design-for-failure (CLAUDE.md)** — provenance resolution is best-effort: a dangling/absent source episode → empty list, a backend Err on a *provenance* read → degrade + `provenance.partial: true` (never cascade the primitive's 200 into a 500); no lock across `.await`.

Anchors the contract cites: `read_as_of` + `{kind}_key`; `lunaris_extract::Fact`; `Chunk.episode_id`; `map_error`; the `{ kind, id, primitive, provenance:{source_episodes,confidence,entities,partial} }` envelope; the `graph_native` signal for entity/relation.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: detail-provenance — single-primitive detail with provenance resolved to source episode(s)
Framings weighed: **primitive + resolved provenance block in one GET** (chosen) · primitive-only, resolve provenance client-side (rejected — provenance IS the inspector's reason to exist) · separate `/provenance/{kind}/{id}` call (rejected — two round-trips for the SPA's lineage drawer)
Must:
<must>
  - `GET /v1/detail/{kind}/{id}` for `kind ∈ episode|chunk|fact|community` → 200 `{ kind, id, primitive, provenance }`, reading the KV row at `{kind}_key(claims.scope, ulid)` via `read_as_of(now)`.
  - `primitive` = the at-rest JSON of the row, **verbatim** (raw `serde_json::Value`, mirroring `episode_handler`'s lenient decode — a non-JSON value degrades to a lossy string, never a 500; a reviewer must be able to SEE a malformed row, not be blocked by it).
  - `provenance.source_episodes` resolves the upstream observation: chunk → `[episode at chunk.episode_id]`; fact → `[episode at source_episode_id]` when that field is present; episode/community → `[]` (an episode IS the source; v0.3 stores no community provenance).
  - `provenance.confidence` = the fact's `confidence` (number) for `kind=fact`; `null` for every other kind.
  - `provenance.entities` = `[subject_id, object_id]` rendered as 32-char lowercase hex (EntityId `Display`) for `kind=fact`; `[]` otherwise.
  - `provenance.partial` (bool, always present) = `true` iff a *provenance* read errored (degraded). **Design-for-failure (CLAUDE.md): provenance resolution is best-effort and NON-CASCADING** — a source-episode read error NEVER downgrades the primitive's 200 into a 500; it only flips `partial`. IO timeouts/retries are owned by the StoragePort driver (same layering as `episode_handler`).
  - A dangling/absent source episode (`read_as_of → None`) contributes nothing to `source_episodes` and does NOT set `partial` (a missing ref is normal data, not a fault).
  - `kind ∈ entity|relation` → 200 `{ kind, id, graph_native: true }` with **NO storage read** (consistent with the frozen `browse-shape-fix` decision; entity/relation detail is served by `graph-endpoint`).
  - scope = JWT `claims.scope` ONLY. The path carries no scope param and the handler reads nothing scope-overridable, so cross-scope access is structurally impossible.
</must>
Reject:
<reject>
  - `kind ∉ {episode,chunk,fact,community,entity,relation}` -> "invalid_kind" (400, no read)
  - `{id}` not a 26-char ULID, for a KV kind -> "invalid_id" (400, no read)
  - valid ULID but no such row in `claims.scope` -> "not_found" (404)
  - primitive `read_as_of` returns `Err` -> "storage" (500, via `map_error`)
  - missing/invalid token -> 401 (the existing `scoped_auth("recall")` layer, before the handler)
</reject>
After:
<after>
  - A reviewer can render the observation→primitive lineage for any KV primitive in their scope from ONE GET.
  - No write occurs anywhere (the route is strictly read-only).
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ The **provenance envelope shape + per-kind semantics** (`source_episodes`/`confidence`/`entities`/`partial`; fact `entities` as 32-hex of subject/object EntityIds; community minimal) — lowest confidence because this is a NEW shape frozen without a second human opinion (fully-auto), and the `inspector-spa` lineage drawer + `graph-endpoint` will bind to it. If wrong: an additive contract bump ripples into task 5's drawer. Mitigation: the shape is purely additive (new keys never break readers); every field is test-pinned; the discriminating test drives the REAL `ingest_structured_inner` write path so the shape can't drift from production.
  - [x] `Ulid` serializes as a 26-char string in the at-rest chunk JSON (so `chunk.episode_id` resolves via `as_str`) — confirmed by the `ulid` serde-as-string convention + `episode_handler` parsing string ids; the real-ingest chunk test proves it green-or-red, not by assertion.
  - [x] `entity`/`relation` belong to `graph_native` here (not 404/400) — confirmed consistent with the frozen `browse-shape-fix` decision; the SPA routes both to `/v1/graph`.
  - [x] structured-ingest writes `subject_id`/`object_id` as the raw `[u8;16]` array (`sid.0`), which is byte-identical to `EntityId`'s derived Serialize — confirmed at `structured_ingest.rs:492/494`, so `from_value::<EntityId>` round-trips for the hex render.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: fact + chunk detail via the REAL ingest path resolve their source episode (DISCRIMINATING)
  Given a single ingest_structured_inner call writes one episode, its chunk, and one fact (source_episode_id stamped)
  When I learn the ids via GET /v1/browse/{fact,episode,chunk} then GET /v1/detail/fact/{fact_id} and /v1/detail/chunk/{chunk_id}
  Then detail/fact is 200 with primitive.fact_text="Alice founded Acme", provenance.confidence≈0.95, provenance.source_episodes[0].id == the episode id, and provenance.entities has two 32-char hex ids
  And detail/chunk's provenance.source_episodes[0].id == the same episode id

Scenario: episode detail returns the primitive with empty provenance
  Given an episode seeded in scope S
  When GET /v1/detail/episode/{id}
  Then 200 with primitive present, provenance.source_episodes=[], confidence=null, entities=[], partial=false

Scenario: community detail returns the primitive with empty provenance
  Given a community seeded in scope S
  When GET /v1/detail/community/{id}
  Then 200 with primitive.summary present, provenance.source_episodes=[], confidence=null, entities=[], partial=false

Scenario: fact with a dangling source episode degrades gracefully
  Given a fact whose source_episode_id points to an episode that was never written
  When GET /v1/detail/fact/{id}
  Then 200 with provenance.source_episodes=[] and partial=false (a missing ref is normal, not a fault)

Scenario: a provenance-read error degrades, never cascades
  Given a fact with a valid source_episode_id but the backend errors on the episode-key read
  When GET /v1/detail/fact/{id}
  Then 200 with primitive present, provenance.source_episodes=[], and partial=true
  And the request is NOT a 500 (the primitive read succeeded)

Scenario: entity detail is graph-native with no storage read
  Given any id
  When GET /v1/detail/entity/{id}
  Then 200 { graph_native: true } and no storage read was performed

Scenario: relation detail is graph-native with no storage read
  Given any id
  When GET /v1/detail/relation/{id}
  Then 200 { graph_native: true } and no storage read was performed

Scenario: cross-scope id is invisible (scope isolation)
  Given a fact seeded ONLY in scope OTHER
  When a scope-S token requests GET /v1/detail/fact/{that_id}
  Then 404 not_found (the read is keyed by claims.scope=S; OTHER's row never leaks)
  And no primitive is returned

Scenario: unknown kind is rejected before any read
  When GET /v1/detail/widgets/{ulid}
  Then 400 invalid_kind
  And no storage read was performed

Scenario: malformed id is rejected before any read
  When GET /v1/detail/fact/not-a-ulid
  Then 400 invalid_id
  And no storage read was performed

Scenario: a valid-but-absent id is not found
  When GET /v1/detail/fact/{unseeded ulid}
  Then 404 not_found
  And no primitive is returned

Scenario: a primitive-read backend error is a 500
  Given the backend errors on the fact-key read
  When GET /v1/detail/fact/{id}
  Then 500 storage
  And no primitive is returned

Scenario: missing token is rejected by the auth layer
  When GET /v1/detail/fact/{id} with no Authorization header
  Then 401
  And the handler is never reached (no storage read)
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
GET /v1/detail/{kind}/{id}      auth: scoped_auth("recall"); scope = JWT claims.scope ONLY; no body
  kind ∈ episode | chunk | fact | community            → KV detail
  kind ∈ entity  | relation                            → graph-native signal

  200 (KV kind) -> {
    kind: "<kind>",
    id:   "<ulid>",
    primitive: { ...at-rest JSON of the row, verbatim... },
    provenance: {
      source_episodes: [ { ...Episode JSON... }, ... ],   # chunk:[0|1] · fact:[0|1] · episode/community:[]
      confidence: <number | null>,                         # fact only; null otherwise
      entities:   [ "<32-hex>", "<32-hex>" ] | [],         # fact: [subject_id, object_id]; else []
      partial:    <bool>                                   # true iff a provenance read errored (degraded)
    }
  }
  200 (entity|relation) -> { kind: "<kind>", id: "<id>", graph_native: true }   # NO storage read
  400 -> { error: "invalid_kind" }    # kind ∉ the six
  400 -> { error: "invalid_id" }      # {id} not a 26-char ULID (KV kinds only)
  404 -> { error: "not_found" }       # no such row in claims.scope
  500 -> { error: "storage" }         # primitive read failed (a provenance failure NEVER 500s — see partial)
  401 -> (auth layer)                 # missing/invalid token, before the handler

Resolution order: (1) kind∈{entity,relation} → graph_native ; (2) kind∉six → 400 invalid_kind ;
                  (3) parse id → 400 invalid_id ; (4) read_as_of(now) → None=404 / Err=500 ;
                  (5) best-effort, non-cascading provenance resolve.
Schema/access: read_as_of(scope, {kind}_key(scope, ulid), clock.tick()); each source-episode resolve =
               read_as_of(scope, episode_key(scope, src), same as_of). No write. No new table/DTO.
```

Status: FROZEN @ v1 — approved by Tin Dang (fully-auto delegation, 2026-06-16)

Least-sure flag surfaced at freeze: [contract] the provenance envelope shape + per-kind semantics —
specifically fact `entities` as 32-char hex of the subject/object `EntityId`s and the `partial`
degradation bool. Why it could be wrong: a reviewer may want richer provenance (relation→episode for
relation-derived facts, or chunk→entity mentions) that v0.3 simply does not store — `structured_ingest`
is explicit that provenance is "episode-level only in v0.3". Cost if wrong: an additive contract bump
rippling into the `inspector-spa` lineage drawer (task 5). Mitigation: the envelope is purely additive
(new keys never break existing readers), every field is test-pinned, and the discriminating test drives
the REAL `ingest_structured_inner` write path, so the shape cannot drift from what production writes.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every §3 resolution branch (all 13 scenarios) hit by ≥1 test.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_detail_fact_and_chunk_resolve_via_real_ingest (DISCRIMINATING): real ingest_structured_inner → browse to learn ids → detail/fact asserts primitive.fact_text + confidence≈0.95 + source_episodes[0].id==episode_id + entities len 2 (each 32-hex); detail/chunk asserts source_episodes[0].id==episode_id
  - test_detail_episode_minimal: seed episode → 200, source_episodes=[], confidence null, entities=[], partial=false
  - test_detail_community_minimal: seed community → 200, primitive.summary present, provenance empty, partial=false
  - test_detail_fact_dangling_source: hand-seed fact-with-source (episode never seeded) → 200, source_episodes=[], partial=false
  - test_detail_fact_provenance_read_fault_partial: hand-seed fact-with-source + read_fault_prefix=episode_key(ep) → 200, primitive present, source_episodes=[], partial=true (NOT 500)
  - test_detail_entity_graph_native: detail/entity/{id} → 200 graph_native:true, read_called==false
  - test_detail_relation_graph_native: detail/relation/{id} → 200 graph_native:true, read_called==false
  - test_detail_cross_scope_404: fact seeded only in OTHER, token S → 404 not_found
  - test_detail_invalid_kind_400: detail/widgets/{ulid} → 400 invalid_kind, read_called==false
  - test_detail_invalid_id_400: detail/fact/not-a-ulid → 400 invalid_id, read_called==false
  - test_detail_not_found_404: detail/fact/{unseeded ulid} → 404 not_found
  - test_detail_primitive_storage_error_500: read_fault_prefix=fact_key(id) → 500 storage
  - test_detail_missing_token_401: no token → 401, read_called==false
</test_plan>

Tests live in: `crates/lunaris-server/tests/detail_provenance.rs` · MUST run red (no route/handler) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-server/src/routes/detail.rs` `crates/lunaris-server/src/routes/mod.rs` `crates/lunaris-server/src/lib.rs` `crates/lunaris-server/tests/detail_provenance.rs`
Strategy (ordered batches): 1. new `routes/detail.rs` with `detail_handler(State, Extension<AuthClaims>, Path<(String,String)>)` — resolution order per §3; 2. `routes/mod.rs` += `pub mod detail;`; 3. `lib.rs` register `GET /v1/detail/{kind}/{id}` mirroring the `/browse/{kind}` block (rate_limit → tracing → `scoped_auth("recall")`).
Safety rule (feature-specific): provenance resolution is best-effort + NON-CASCADING — a source-episode read `Err` sets `partial=true`, never a 500; only the primitive read drives 200/404/500. No write ops are ever issued.
Code lives in: `crates/lunaris-server/src/`
Constraints: do NOT change any test or the contract; allow-list packages only (no new external dep — `lunaris-extract` already present); ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — `cargo test -p lunaris-server` = 119 passed / 3 ignored (12 suites); `detail_provenance` 13/13.
- [x] coverage did not decrease — +1 suite (13 new tests), all green; no existing test removed.
- [x] no test or contract was altered during build — the only post-red edits to the test file were (a) removing two genuinely-dead `Scope` locals and (b) `cargo fmt` wrapping one long assertion; zero assertions changed. §3 contract unchanged.
- [x] the green was EARNED, not gamed — adversarial refute-read (self): the discriminating test drives the REAL `ingest_structured_inner` and matches the at-ingest-minted `ep_id`, so a stub can't pass; the `partial` test kills both a cascading-500 impl and a silent-swallow impl; graph_native + rejects assert `read_called==false` (kills read-then-discard); cross-scope asserts 404 (kills scope leak). No vacuous asserts, no fixture overfit.
- [x] concurrency / timing safe — no lock held across `.await`; the handler holds no storage guard (the `MockStorage`/backend owns its own locking); provenance reads are sequential awaits with no shared mutable state.
- [x] no exposed secrets, injection openings, or unexpected dependencies — keys are built from a validated `Ulid` + `claims.scope` (no string interpolation into a query); read-only (no `WriteOp` issued); no new crate dependency (`lunaris-extract` already present).
- [x] layering & dependencies follow CONVENTIONS.md — handler reads `claims.scope` ONLY (JWT is the sole scope source); reuses `keyspace::{kind}_key` from `lunaris-core` (no local key-mint); reuses `map_error`; `read_as_of` "now" via `clock().tick()` mirrors `episode_handler`.
- [x] a person reviewed and approved the change — fully-auto delegation (Tin Dang, 2026-06-16); auto-PASS on complete evidence per `autonomy: auto`. NO security finding (scope isolation proven by `test_detail_cross_scope_404`), so no HARD-STOP escalation.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `detail_handler` is referenced by the `/detail/{kind}/{id}` route in `lib.rs`; `resolve_provenance` is called by `detail_handler`; `routes::detail` is declared in `routes/mod.rs` and used in `lib.rs`. The discriminating test exercises the production read path end-to-end through the registered route.
- [x] DEAD-CODE (code) — no new unused/orphaned symbol; `cargo clippy -p lunaris-server --all-targets -D warnings` = clean (caught + cleared the dead test locals + a doc-overindent before this gate).
- [N/A] SEMANTIC (prose / non-code) — this is a code task.

### GATE RECORD
Outcome: PASS
If RISK-ACCEPTED -> owner: <name> · ticket: <link> · expires: <date>   (never for a security gap)
Reviewed by: Tin Dang (fully-auto delegation) · date: 2026-06-16

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
