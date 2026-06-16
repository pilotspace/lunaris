# TASK: Fix browse/{kind} to the real at-rest shapes (fact=extract::Fact; entity/relation graph-native)

slug: browse-shape-fix · created: 2026-06-16 · stage: production
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

The shipped `browse-endpoints` task used `scan_page::<lunaris_core::{Fact,Entity,Relation}>`. Tracing every ingest path shows the at-rest KV shapes DIFFER from the core primitives — so browse/fact 500s and browse/entity|relation return empty against production data. This task fixes browse to the real shapes.

At-rest KV reality (traced 2026-06-16):
- `crates/lunaris-ingest/src/pipeline.rs:313-378` — main pipeline writes ONLY `doctree:`/`episode:`/`chunk:` KvPut (+ chunk VectorUpsert). No fact/entity/relation KV.
- `crates/lunaris/src/ingest.rs:474-532` — the extractor-driven ingest writes **entities as `WriteOp::GraphNode`** and **relations as `WriteOp::GraphEdge`** (NOT KvPut); facts as `KvPut` of `serde_json::to_vec(&validated.facts[i])` where the element is `lunaris_extract::types::Fact` (`crates/lunaris-extract/src/types.rs:237`): `{ id: Ulid, subject_id: EntityId, predicate, object_id: EntityId, fact_text, confidence, valid_from_iso, valid_to_iso }` — NO `scope`/`bt`/`provenance`/`activation` → `core::Fact` deser FAILS.
- `crates/lunaris/src/structured_ingest.rs:489-499` — agent-supplied `ingest_structured` writes `fact:` rows as a `json!` with the SAME extract fields PLUS `source_episode_id` (extra field; `extract::Fact` has no `deny_unknown_fields` so it deserializes fine and ignores it).
- `crates/lunaris-ingest/src/chunker/raptor.rs:115`,`294` — communities ARE KvPut at `community_key` as `core::Community`. ✓ browse/community already correct.
- `crates/lunaris-verify/src/worker.rs:330` — the ONLY writer of `entity:`/`relation:` KV keys, and only as supersede/soft-delete tombstones — NOT the primary store.
- Net: KV-backed browsable kinds are `episode` (`core::Episode`), `chunk` (`core::Chunk`), `community` (`core::Community`), `fact` (`lunaris_extract::Fact`). `entity`/`relation` are GRAPH-native (no primary KV store).

Touches (files · symbols · signatures):
- `crates/lunaris-server/src/routes/browse.rs:browse_handler` — the kind→type dispatch (shipped). Change `fact` arm to `scan_page::<lunaris_extract::types::Fact>`; change `entity`/`relation` arms to return a typed graph-native response (no scan).
- `crates/lunaris-server/Cargo.toml` — add `lunaris-extract = { workspace = true }` (server is the top layer; `lunaris` already pulls it transitively). Used for `lunaris_extract::types::Fact`.
- `crates/lunaris/src/handle.rs:1232` `Lunaris::ingest_structured(StructuredIngest) -> Result<Lsn>` + `crates/lunaris/src/structured_ingest.rs:150 FactInput` — the REAL ingest entrypoint the discriminating test drives to write a production-shape `fact:` row.
- `crates/lunaris-server/tests/browse_endpoints.rs` — the shipped suite; its `core::Fact`-seeded fact assertions are corrected to the real shape, and a NEW discriminating test seeds via `ingest_structured`.

Context (working folder): `crates/lunaris-server/src/routes/browse.rs` (handler edit) + `Cargo.toml` dep + the test file. No new routes, no DTO changes, no migrations.

Honors (patterns / conventions): no duplicate fact DTO — reuse `lunaris_extract::types::Fact` (CLAUDE.md "no duplicate libs/types" ethos); JWT `claims.scope` only; `map_error` envelope shape; `built ≠ wired` — the new test exercises the production ingest path, not a hand-seeded row.

Anchors the contract cites: `lunaris_extract::types::Fact`; `scan_page`; `Lunaris::ingest_structured`; the six `*_prefix` helpers; the existing `{ items, next_cursor }` envelope + `ListError.code()` map; the NEW `graph_native` typed response for entity/relation.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: correct `GET /v1/browse/{kind}` to the REAL at-rest shapes so it works against production data — `fact` deserializes `lunaris_extract::Fact`; `entity`/`relation` are answered as graph-native (no bogus empty KV page); `episode`/`chunk`/`community` are already correct.
Framings weighed: per-kind dispatch keeps KV kinds on `scan_page::<T>` with the correct `T` and special-cases the two graph kinds with a typed signal (chosen — honest to the data model, smallest diff, dashboard can route to the graph view) · scan the graph for entity/relation to fake a KV-style page (rejected — duplicates the graph-endpoint task, pagination over Cypher is awkward, blurs the read model) · drop entity/relation from the kind enum entirely so they 400 invalid_kind (rejected — they ARE valid memory kinds; conflating "graph-native" with "not a kind" misleads the client).
Must:
<must>
  - `GET /v1/browse/fact` deserializes the PRODUCTION fact row `lunaris_extract::types::Fact` and returns `200 { items:[<that JSON>…], next_cursor }`. It MUST accept facts written by BOTH ingest paths: the extractor path (`ingest.rs`) and `ingest_structured` (the latter carries an extra `source_episode_id`, which is tolerated, not rejected).
  - `GET /v1/browse/{episode|chunk|community}` is UNCHANGED — still `scan_page::<core::{Episode|Chunk|Community}>` (these shapes already match at-rest).
  - `GET /v1/browse/{entity|relation}` returns `200 { items: [], next_cursor: null, graph_native: true }` and performs NO scan — these kinds live in the graph, not KV. The `graph_native: true` discriminator tells the SPA to route to `GET /v1/graph` instead of rendering an empty table as "no data" (Tin-confirmed @ freeze).
  - All pre-existing browse Musts still hold for the KV kinds: scope = `claims.scope` only, ULID-ascending, forward cursor with no skip/repeat, default `limit` 20, cap `MAX_PAGE`. KV-kind 200 responses do NOT carry `graph_native` (absent = false).
  - A discriminating test seeds a `fact:` row via the REAL `Lunaris::ingest_structured` path (not a hand-seeded `core::Fact`), then asserts `browse/fact` returns it 200 — proving the endpoint reads what production writes (built ≠ wired).
</must>
Reject:
<reject>
  - `{kind}` not one of the six -> 400 "invalid_kind"   (unchanged)
  - `limit == 0` -> 400 "invalid_limit" · `limit > MAX_PAGE` -> 400 "limit_too_large" · malformed `cursor` -> 400 "invalid_cursor"   (unchanged, from scan_page)
  - a stored `fact:` (or episode/chunk/community) row that fails to deserialize into its real type -> 500 "corrupt_row"
  - backend / storage error -> 500 "storage"
</reject>
After:
<after>
  - `browse/fact` round-trips real facts; `browse/{entity,relation}` never fabricates an empty KV page; no write occurs; the error envelope stays `map_error`'s `{ error, message }`.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ [x] DECIDED @ freeze v1 — entity/relation browse answers **`200 { items: [], next_cursor: null, graph_native: true }`** (confirmed by Tin Dang; chosen over 409 and over enumerating the graph). The SPA MUST branch on `graph_native === true` to route to `/v1/graph` rather than show an empty table. Residual: if a later milestone wants real entity/relation enumeration, it enumerates the graph (graph-endpoint territory) — a superset, not a breaking change.
  - [ ] CONFIRMED (this session) — `browse/fact` target type is `lunaris_extract::types::Fact`; FORCED by the at-rest data (not a choice).
  - [ ] CONFIRMED (this session) — episode/chunk/community stay on the core primitives; FORCED by the at-rest data.
  - [ ] the discriminating test uses `ingest_structured` (it deterministically writes a `fact:` KV row with a `NoopExtractor`/no-model dependency); the extractor path needs a live model so it is NOT used in the test — `ingest_structured` is a real production write path, so the discriminator is valid.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
# ── Musts ──────────────────────────────────────────────────────────────
Scenario: Browse fact reads a fact written by the REAL ingest path (discriminating)
  Given a token bound to scope S and a fact ingested via Lunaris::ingest_structured under S
  When GET /v1/browse/fact
  Then 200 and items[0] is that fact's stored JSON (fact_text + confidence present)
  And the endpoint deserialized the production lunaris_extract::Fact shape, not core::Fact

Scenario: Browse fact accepts the extractor-path shape (no source_episode_id)
  Given scope S holds a fact-prefixed row that is a valid extract::Fact WITHOUT source_episode_id
  When GET /v1/browse/fact
  Then 200 with that fact present (both ingest shapes deserialize)

Scenario: Browse fact still pages and orders by ULID
  Given scope S holds 3 extract::Fact rows (u1<u2<u3) and a token bound to S
  When GET /v1/browse/fact?limit=2 then ?limit=2&cursor=<next_cursor>
  Then [u1,u2] then [u3], next_cursor null on page 2, none skipped/repeated

Scenario: KV-native kinds are unchanged
  Given scope S holds one episode, one chunk, one community
  When GET /v1/browse/{episode|chunk|community}
  Then each returns 200 with its core-primitive item present
  And the response has no graph_native flag (absent = false)

Scenario: Entity browse is graph-native (no scan)
  Given a valid token
  When GET /v1/browse/entity
  Then 200 { items: [], next_cursor: null, graph_native: true }
  And no scan is performed (these live in the graph; the SPA routes to GET /v1/graph)

Scenario: Relation browse is graph-native (no scan)
  Given a valid token
  When GET /v1/browse/relation
  Then 200 { items: [], next_cursor: null, graph_native: true }
  And no scan is performed

# ── Rejects (each asserts what stays unchanged) ─────────────────────────
Scenario: A corrupt fact row surfaces as 500
  Given scope S has a fact-prefixed key whose value is not a valid extract::Fact
  When GET /v1/browse/fact
  Then 500 { error: "corrupt_row" }
  And no partial page is returned as if complete

Scenario: Unknown kind is still rejected
  Given a valid token
  When GET /v1/browse/widgets
  Then 400 { error: "invalid_kind" }
  And no scan is performed
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
GET /v1/browse/{kind}?cursor=&limit=     auth: Bearer · scoped_auth("recall")   [shape-correction of browse-endpoints v1]

  KV-backed kinds (scan_page::<T> → { items, next_cursor }, scope=claims.scope only, ULID-asc, limit default 20 cap MAX_PAGE):
    episode   -> T = lunaris_core::Episode      (unchanged)
    chunk     -> T = lunaris_core::Chunk         (unchanged)
    community -> T = lunaris_core::Community      (unchanged)
    fact      -> T = lunaris_extract::types::Fact   (CHANGED from core::Fact; tolerates the extra
                 source_episode_id that ingest_structured writes — extract::Fact has no deny_unknown_fields)
    200 -> { items: [<full at-rest JSON of T>…], next_cursor: string | null }

  GRAPH-native kinds (NO scan):
    entity | relation -> 200 { items: [], next_cursor: null, graph_native: true }   # SPA routes to GET /v1/graph
    (KV-kind 200 responses do NOT carry graph_native — absent = false)

  unknown {kind}                       -> 400 { error: "invalid_kind" }     (handler, pre-scan; unchanged)
  limit/cursor from scan_page          -> 400 { error: "invalid_limit" | "limit_too_large" | "invalid_cursor" }
  row fails to deserialize into T      -> 500 { error: "corrupt_row" }
  backend / storage error              -> 500 { error: "storage" }
  401 -> (no/invalid token — existing auth layer)

Schema/access: READ-ONLY; no new routes, no DTO changes, no migrations. Adds dep
  lunaris-extract (workspace) to lunaris-server for the real fact type. Envelope = map_error's { error, message }.
  /v1/scopes is untouched by this task.
```

Least-sure flag surfaced at freeze: [contract] entity/relation browse → `200 { items:[], next_cursor:null, graph_native:true }` (chosen over 409 and over graph enumeration). The SPA MUST branch on `graph_native === true`; if it ignores the flag it renders an empty table as "no data". Cost if wrong: a one-line shape change here + the SPA picker handler.

Status: FROZEN @ v1 — approved by Tin Dang (2026-06-16); entity/relation → 200 + graph_native flag confirmed. Changing this contract = a change request back to SPECIFY.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: 90%. Same harness as the shipped `browse_endpoints.rs` (configurable `MockStorage` + real `build()` router via `oneshot`). The existing fact tests that seed `core::Fact` are RE-SHAPED to the real `extract::Fact` row, the entity/relation arm of `test_each_kind_browsable` is split out into the two graph-native tests, and ONE new discriminating test drives the real `ingest_structured` path. Delta = "re-shape + add"; the unchanged episode/chunk/community + reject tests stay.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_browse_fact_via_real_ingest_structured (NEW, discriminating): construct a Lunaris over MockStorage, call lunaris.ingest_structured(payload with 1 entity-pair + 1 fact) under scope S, then GET /v1/browse/fact / 200, items[0].fact_text == payload fact_text, confidence present — proves browse reads the production-written row (built ≠ wired)
  - test_browse_fact_extractor_shape: seed a fact: row = extract::Fact WITHOUT source_episode_id / GET /v1/browse/fact / 200 with it present
  - test_browse_fact_paginates (re-shape of test_browse_fact_scoped_ordered_page + _pagination_walks_once): seed 3 extract::Fact rows u1<u2<u3 / page limit=2 then cursor / [u1,u2] then [u3], next_cursor null on p2, scope==S
  - test_each_kv_kind_browsable (re-shape): seed one episode/chunk/community / GET each / 200 with item; assert NO graph_native flag
  - test_browse_entity_graph_native (NEW): GET /v1/browse/entity / 200 { items:[], next_cursor:null, graph_native:true }, scan_called==false
  - test_browse_relation_graph_native (NEW): GET /v1/browse/relation / 200 graph_native:true, scan_called==false
  - test_corrupt_row_500 (re-shape): seed a fact: key with non-extract::Fact JSON / GET /v1/browse/fact / 500 corrupt_row
  - test_invalid_kind_400 (unchanged): GET /v1/browse/widgets / 400 invalid_kind, scan_called==false
  - retained unchanged: test_zero_limit_400, test_over_cap_limit_400, test_malformed_cursor_400, test_storage_error_500, test_missing_token_401, test_scopes_* (the /scopes tests are untouched by this task)
</test_plan>

Tests live in: `crates/lunaris-server/tests/browse_endpoints.rs` (re-shape in place) · MUST run red (fact deser + graph_native + discriminating test fail) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-server/src/routes/browse.rs` (the kind dispatch) · `crates/lunaris-server/Cargo.toml` (add `lunaris-extract` workspace dep) · `crates/lunaris-server/tests/browse_endpoints.rs` (test re-shape — declared so the build-phase edit is in-scope, unlike the browse-endpoints clippy-fix that tripped the scope gate)
Strategy (ordered batches): 1. add `lunaris-extract` dep to Cargo.toml 2. browse.rs: `fact` arm → `scan_page::<lunaris_extract::types::Fact>`; `entity`/`relation` arms → early-return `200 { items:[], next_cursor:null, graph_native:true }` (no scan); episode/chunk/community unchanged 3. re-shape the test fixtures (extract::Fact) + add the 3 new tests 4. green the suite
Safety rule (feature-specific): scope stays `claims.scope`-only; graph-native arms must NOT touch storage; KV 200s must NOT carry `graph_native`; reuse the existing envelope + map_error.
Code lives in: `crates/lunaris-server/src/`
Constraints: do NOT change the frozen contract; allow-list packages only (lunaris-extract is already a transitive workspace member); ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — `cargo test -p lunaris-server --test browse_endpoints` 17/17 (was 13; +4: discriminating-ingest, extractor-shape, entity-graph-native, relation-graph-native); full crate 106 passed / 3 ignored.
- [x] coverage did not decrease — net +4 tests; every changed dispatch arm (fact, entity, relation) is exercised; KV kinds + all rejects retained.
- [x] no test or contract was altered during build — build touched ONLY `browse.rs`. The test re-shape + the `lunaris-extract` dep were made during the TESTS phase (before the tests→build scope snapshot); the frozen contract is unchanged.
- [x] the green was EARNED — the anti-"built ≠ wired" guard IS the headline test: `test_browse_fact_via_real_ingest_structured` drives `ingest_structured_inner` (the exact fn the handle delegates to) to WRITE the fact row, then asserts browse reads it — it went 500→200 across the fix, proving the endpoint now reads the production shape, not a hand-seeded one. Not gamed: `corrupt_row` still forces typed `extract::Fact` deser; graph-native tests assert `scan_called == false` (no fake-empty via an empty scan).
- [x] concurrency / timing safe — no lock held across `.await`; graph-native arms early-return without touching storage; no new shared state.
- [x] no exposed secrets, injection openings, or unexpected dependencies — the only new dep is `lunaris-extract`, already a transitive workspace member (server is the top layer); no external dep; scope still `claims.scope`-only.
- [x] layering & dependencies follow CONVENTIONS.md — reuses the real `lunaris_extract::Fact` (no duplicate DTO); `scan_page`/keyspace from `lunaris-core`; `map_error` envelope; the graph-native flag is additive (KV 200s omit it).
- [ ] a person reviewed and approved the change — auto-gated under `autonomy: auto`; the contract (entity/relation → 200 + `graph_native`) was human-frozen by Tin this session.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `browse_handler`'s `fact` arm now pages `ExtractFact`; `entity`/`relation` early-return the graph-native body; episode/chunk/community unchanged. Confirmed live: the 17-test suite (incl. the real-ingest discriminator) drives every arm through `build()`.
- [x] DEAD-CODE (code) — removed the now-unused `entity_prefix`/`relation_prefix` + core `Entity`/`Fact`/`Relation` imports; clippy `-p lunaris-server --all-targets -D warnings` clean (would flag unused imports / `unreachable_pub`).
- [ ] SEMANTIC (prose / non-code) — n/a (code change).

### GATE RECORD
Outcome: PASS (auto-resolved under `autonomy: auto` — complete evidence; the discriminating real-ingest test closes the built ≠ wired gap; no security/concurrency/architecture residue)
If RISK-ACCEPTED -> owner: — · ticket: — · expires: —   (never for a security gap)
Reviewed by: auto-gate (engine); contract human-frozen by Tin Dang · date: 2026-06-16

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): browse/fact `corrupt_row` rate (a spike = an ingest path writing a shape browse can't read — a schema-drift early warning); browse/{entity,relation} call volume (how often the SPA hits graph-native kinds → demand signal for real graph enumeration).
Spec delta for the next loop: the at-rest read model is heterogeneous — `core` primitives for episode/chunk/community, `extract::Fact` for facts, the GRAPH for entities/relations. Any future read surface (detail-provenance, exports) MUST consult this map, not assume `core` primitives. detail-provenance is already scoped accordingly (KV kinds only; fact provenance = `source_episode_id` when present).

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
- [TDD · open] browse-endpoints shipped green but prod-broken because its tests hand-seeded `core::Fact` rows instead of driving the real ingest path — the exact built ≠ wired trap. Every read-surface test MUST seed via a production write path (here `ingest_structured_inner`). (evidence: `test_browse_fact_via_real_ingest_structured` went 500→200 across this fix; the old suite never caught the 500.)
- [DDD · open] the domain has THREE at-rest shapes for "memory" (core primitives / extract::Fact / graph nodes+edges); the `core` primitives are an aspirational model, not the on-disk truth. The keyspace exposes `*_prefix` helpers for all six kinds even though entity/relation are never KV-populated by the happy path — a helper's existence ≠ data behind it. (evidence: traced ingest.rs/structured_ingest.rs/raptor.rs/verify worker 2026-06-16.)
- [SDD · open] a task premise can be contradicted by the codebase ("resolve provenance: Vec<Ulid>" — a field never serialized to disk); grounding surfaced it BEFORE a contract froze on it, and the human re-scoped. The ground phase earned its keep. (evidence: detail-provenance re-scoped + this fix-forward task spawned.)
