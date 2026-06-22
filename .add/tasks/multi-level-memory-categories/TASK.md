# TASK: Typed user/session/agent memory levels + categories + metadata-filter API

slug: multi-level-memory-categories · created: 2026-06-15 · stage: production
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

> Grounded on `feat/memory-inspector` 2026-06-17. Feature confirmed GENUINELY UNBUILT on current main:
> `lunaris_core::Scope` (scope.rs) has only `new/from_trusted/dev/as_str/as_bytes` — NO `child`/`compose_levels`/
> `MemoryLevel`; `IngestBody{id,source,content,t_ref,metadata}` + `RecallRequest{query,k,as_of,filter,mode}` carry
> NO `user_id`/`agent_id`/`session_id`/`categories`. A prior branch `feat/multi-level-memory-categories`
> (red/green `9f25cd4`+`999533f`, branched at merge-base `0794385`, now 32 commits behind HEAD) is a COMPLETE
> reference implementation (Fork B = levels as TRUE sub-partitions under the JWT base scope, chosen by Tin) but was
> never re-verified on current main — treated as reference, NOT trusted (per [[feedback_built_not_wired]]).

Touches (files · symbols · signatures):
  - `crates/lunaris-core/src/scope.rs` — `Scope` (newtype over validated `String`; alphabet `VALID_SCOPE_CHARS = [A-Za-z0-9_\-.]{1,128}`, `:` REJECTED so `lunaris:{scope}:{kind}:{ulid}` can't byte-alias; `MAX_SCOPE_LEN`). Hand-rolled validating `Deserialize` (NOT derived). Feature ADDS: `Scope::child(segment)` (+ a private `is_valid_segment` whose alphabet must EXCLUDE `.`/`:`/`/` so a segment can't forge a level tag or alias the KV format), `MemoryLevel{User,Agent,Session}` (`u-`/`a-`/`s-` tags), `compose_levels(base,user,agent,session)` (canonical order; all-None → `base.clone()`).
  - `crates/lunaris-core/src/lib.rs` — crate-root re-exports (DRIFTED +2 since merge-base; new `MemoryLevel`/`compose_levels` exports go here).
  - `crates/lunaris-server/src/dto.rs` — `IngestBody` (:21) + `RecallRequest` (:51), both `#[serde(deny_unknown_fields)]`. `RecallRequest` ALREADY has `filter: Option<String>` → `RetrievalBuilder::filter_str` (the AND-target for categories). Feature ADDS optional `user_id`/`agent_id`/`session_id` + `categories: Vec<String>` to both, plus helper fns `compose_request_scope` / `validate_categories` / `categories_filter`. (DRIFTED +65 since merge-base = the memory-inspector `BrowseQuery`/`ScopesQuery`/`GraphQuery` DTOs — a DIFFERENT region of the file, so the prior additions still slot in, but the helper placement + deny_unknown_fields interaction need re-verification — this is the integration-risk region.)
  - `crates/lunaris-server/src/routes/ingest.rs` — `ingest_handler` (:38): `Extension(claims): Extension<AuthClaims>` → `state.lunaris.scoped(claims.scope.clone())` + `queue_depth(&claims.scope, …)`. UNCHANGED since merge-base. Feature composes `claims.scope` + body level-ids → composed scope, `.scoped(composed)`, and stores `Episode.metadata["categories"]`.
  - `crates/lunaris-server/src/routes/recall.rs` — `recall_handler` (:33): `state.lunaris.scoped(claims.scope.clone()).dsl()` then applies optional `req.filter` via `builder.filter_str` (:98). UNCHANGED since merge-base. Feature composes the SAME partition + AND-combines a categories `Filter` into the existing filter.
  - `crates/lunaris-retrieve/src/builder.rs:180` `RetrievalBuilder::filter_str` → `crates/lunaris-retrieve/src/operators/modifiers.rs:80` `filter_str(&str) -> Result<Filter, FilterParseError>` — the v0 string-DSL the categories filter must compose with (Eq/Or on a `"categories"` field).
Context (working folder): pure feature work, no new deps. Mem0-parity surface = typed user/session/agent memory levels + custom categories + a metadata-filter recall API. Reference impl on `feat/multi-level-memory-categories` (1241 insertions / 9 files; scope.rs/ingest.rs/recall.rs replay cleanly, dto.rs is the drift region). Security model from memory: JWT base scope is ALWAYS a strict prefix of the composed scope → a caller can only NARROW, never escape (proven there by `compose_never_escapes_base`).
Honors (patterns / conventions): RFC 0001 — keyspace helpers live in `lunaris_core` ONLY; `lunaris:{scope}:{kind}:{ulid}` format; `:` excluded from the Scope alphabet (type-level closure); never derive `Deserialize` on the `Scope` newtype (hand-roll via the validating constructor). HTTP DTO discipline — every public request DTO keeps `#[serde(deny_unknown_fields)]`; the JWT `claims.scope` is the ONLY partition-scope source (new level-id fields NARROW under it, never replace it; wire `scope`/`tenant` still 422). `Scope::dev()` is a test/migration crutch — thread the real scope.
Anchors the contract cites: `Scope::child` / `compose_levels` / `MemoryLevel` (new in `lunaris-core`); the new optional `IngestBody`/`RecallRequest` fields (`user_id`/`agent_id`/`session_id`/`categories`) + dto helpers `compose_request_scope` / `validate_categories` / `categories_filter`; `RetrievalBuilder::filter_str` as the categories AND-target.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Typed user/agent/session memory levels + custom categories + a metadata-filter recall API. The HTTP ingest/recall surface gains optional `user_id`/`agent_id`/`session_id` (typed sub-partitions composed UNDER the JWT base scope) and `categories` (cross-cutting tags stored on the episode + AND-filterable at recall). Mem0 parity: per-level memory + category filtering, but built on Lunaris's `:`-delimited keyspace so isolation is type-level, not advisory.
Framings weighed: **Fork B — levels as TRUE sub-partitions under the JWT base scope** (chosen; Tin's prior decision — isolation falls out of the `lunaris:{scope}:{kind}:{ulid}` prefix, the base stays a strict prefix so a caller can only NARROW, never escape) · Fork A — levels as an `Episode.metadata` field filtered at recall (rejected — a metadata filter is bypassable / not a real partition; gives no tenancy guarantee) · categories-as-partition (rejected — categories are cross-cutting tags, not a partition axis; they belong as a filterable metadata array, NOT in the scope string)
Must:
<must>
  - `lunaris_core::Scope::child(segment) -> Result<Scope, ScopeError>` appends `.{segment}` to the scope (the `.` separator is in `VALID_SCOPE_CHARS`); `segment` must match the level-segment alphabet `[A-Za-z0-9_-]+` (EXCLUDES `.`/`:`/`/`) so a segment can neither forge a level boundary nor alias the `:` KV delimiter. Composed length > `MAX_SCOPE_LEN` is rejected.
  - `MemoryLevel{User,Agent,Session}` carry distinct tag prefixes (`u-`/`a-`/`s-`); `compose_levels(base, user, agent, session)` applies `child` in CANONICAL order (user → agent → session), tagging each present id; all-None → `base.clone()`.
  - For ANY subset of present level-ids, the composed scope has `base.as_str()` as a STRICT PREFIX (the load-bearing no-escape invariant) and is distinct per (level, id) — two different ids never collide, and a `u-` id never aliases an `a-`/`s-` id.
  - `POST /v1/ingest` accepts optional `user_id`/`agent_id`/`session_id` + `categories`: the episode is written under the COMPOSED scope (base + levels), and `categories` are stored on `Episode.metadata["categories"]` as a JSON array. `deny_unknown_fields` stays; a wire-side `scope`/`tenant` is still 422.
  - `POST /v1/recall` accepts the same optional level-ids + `categories`: recall queries the SAME composed partition; when `categories` is non-empty it AND-combines an Eq/Or-on-`"categories"` `Filter` into the existing `req.filter` (via `RetrievalBuilder::filter_str`). A recall at the base scope NEVER sees a child-scope episode, and vice-versa.
  - `categories` are validated: ≤ 16 items, each 1..=64 bytes (non-empty).
</must>
Reject:
<reject>
  - A level-id containing `.`/`:`/`/`, empty, or outside `[A-Za-z0-9_-]` -> "invalid_level_segment"
  - A composed scope exceeding `MAX_SCOPE_LEN` -> "scope_too_long"
  - `categories` with > 16 items, or any item empty or > 64 bytes -> "invalid_categories"
  - A wire-side `scope` / `tenant` field (override attempt) -> existing 422 `deny_unknown_fields` (UNCHANGED — must not regress)
</reject>
After:
<after>
  - Ingest with `user_id="alice"` under base `T` writes under composed scope `T.u-alice`; a base-`T` recall does NOT return it; a recall with `user_id="alice"` does.
  - The composed scope always has the JWT base as a strict prefix — no id value escapes the tenant (property-checked over hostile ids × positions).
  - `categories` round-trip: stored on the episode at ingest and usable as an AND filter at recall; `deny_unknown_fields` + the JWT-scope-only rule intact on both DTOs.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ [contract] The categories AND-filter actually round-trips on the PRODUCTION recall path (Moon backend) — lowest confidence because there is a KNOWN class of "hybrid filter bypass" bug where a metadata/`filter_str` predicate is silently dropped on Moon (see [[moon-hybrid-filter-bypass]] / the scratchpad_read serde bug in [[reference_recall_optimization_validation]]), so `categories` could silently return everything (filter ignored) or nothing (over-filtered) rather than the intended subset. If wrong: the category feature is built-but-not-wired (returns wrong set) while unit tests pass — the exact built-≠-wired trap ([[feedback_built_not_wired]]); mitigation = a DISCRIMINATING recall test that ingests two episodes differing only by category and asserts the filter actually partitions them on the real backend, not just a parse-level assert.
  - [ ] The level encoding (`.` separator + `u-`/`a-`/`s-` tags, segment `[A-Za-z0-9_-]+`) is the right PERMANENT keyspace commitment — confirmed by the prior branch's `compose_never_escapes_base` property test (11 hostile ids × 4 positions); if wrong, stored child-scope data orphans → a migration, but the strict-prefix + `:`-non-alias proof holds at the type level.
  - [ ] Categories live in `Episode.metadata["categories"]` (a JSON array) rather than a dedicated secondary index — accepted for v1 (matches the reference impl); a dedicated category index is a perf follow-up, not parity-blocking.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: composed scope keeps the base as a strict prefix (no escape)
  Given a base Scope "T" and hostile level-ids (e.g. "../x", "a:b", "", "u-evil")
  When compose_levels / Scope::child is applied with any present subset
  Then either an error is returned (invalid_level_segment) for an illegal id
  And every successfully composed scope string starts with "T" as a strict prefix
  And the base "T" itself is unchanged

Scenario: compose_levels canonical order and all-None passthrough
  Given base "T"
  When compose_levels(T, user="alice", agent="bot", session="s1") is called
  Then the result is "T.u-alice.a-bot.s-s1" (canonical user→agent→session order)
  And compose_levels(T, None, None, None) returns "T" unchanged

Scenario: ingest writes under the composed scope; base recall is isolated
  Given an authenticated caller bound to base scope "T"
  When it ingests with user_id="alice"
  Then the episode is stored under composed scope "T.u-alice"
  And a recall at base "T" (no level-ids) does NOT return that episode
  And a recall with user_id="alice" DOES return it

Scenario: categories AND-filter actually partitions results (discriminating)
  Given two episodes ingested under the same composed scope, identical except category ("blue" vs "green")
  When recall is issued with categories=["blue"] on the real backend
  Then only the "blue" episode is returned
  And the "green" episode is excluded (the filter is applied, not silently dropped or over-applied)

Scenario: categories stored on the episode at ingest
  Given an ingest body with categories=["work","urgent"]
  When the episode is persisted
  Then Episode.metadata["categories"] is the JSON array ["work","urgent"]
  And the rest of the metadata is unchanged

Scenario: reject an illegal level-id
  Given an ingest/recall body with user_id="a.b" (contains the separator)
  When the request is handled
  Then it is rejected with error "invalid_level_segment"
  And no episode is written / no partition is created

Scenario: reject a composed scope that is too long
  Given level-ids whose composed length exceeds MAX_SCOPE_LEN
  When compose is attempted
  Then it is rejected with error "scope_too_long"
  And the base partition is untouched

Scenario: reject malformed categories
  Given a body with 17 categories, or a category of "" or > 64 bytes
  When the request is handled
  Then it is rejected with error "invalid_categories"
  And no episode is written

Scenario: wire-side scope/tenant override still 422 (no regression)
  Given an ingest/recall body carrying a "scope" or "tenant" field
  When the request is deserialized
  Then it is rejected 422 by deny_unknown_fields
  And the JWT-bound scope remains the only partition source
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
CORE  (crates/lunaris-core/src/scope.rs — additive; Scope newtype unchanged)
  Scope::child(&self, segment: &str) -> Result<Scope, ScopeError>
    Ok  -> Scope("{self}.{segment}")        // '.' separator; segment ∈ [A-Za-z0-9_-]+
    Err -> ScopeError::InvalidSegment        // empty / contains '.'/':'/'/' / non-alphabet char
    Err -> ScopeError::TooLong               // composed len > MAX_SCOPE_LEN (128)
  enum MemoryLevel { User, Agent, Session }  // tag(): "u-" | "a-" | "s-"
  compose_levels(base: &Scope, user: Option<&str>, agent: Option<&str>, session: Option<&str>)
        -> Result<Scope, ScopeError>
    // applies child(tag()+id) in canonical order user→agent→session; all-None -> base.clone()
    // INVARIANT: base.as_str() is a strict prefix of the result (no-escape, property-tested)
  (re-exported at lunaris_core crate root: MemoryLevel, compose_levels)

HTTP  POST /v1/ingest   body: { id?, source, content, t_ref?, metadata?,
                                 user_id?, agent_id?, session_id?, categories? }
  201 -> { lsn, queue_lag_warn }            // UNCHANGED IngestResponse; episode stored under
                                            //   compose_levels(claims.scope, user_id, agent_id, session_id),
                                            //   categories -> Episode.metadata["categories"] (JSON array)
  400 -> { error: "invalid_level_segment" | "scope_too_long" | "invalid_categories" }
  422 -> deny_unknown_fields (wire "scope"/"tenant" — UNCHANGED)

HTTP  POST /v1/recall   body: { query, k?, as_of?, filter?, mode?,
                                 user_id?, agent_id?, session_id?, categories? }
  200 -> { hits: [...] }                    // queried under the SAME composed partition;
                                            //   categories (non-empty) AND-combined into req.filter as
                                            //   Eq/Or on "categories" via RetrievalBuilder::filter_str
  400 -> { error: "invalid_level_segment" | "scope_too_long" | "invalid_categories" | "<filter parse err>" }
  422 -> deny_unknown_fields (UNCHANGED)

DTO helpers (crates/lunaris-server/src/dto.rs):
  compose_request_scope(base, user_id, agent_id, session_id) -> Result<Scope, (StatusCode, code)>
  validate_categories(&[String]) -> Result<(), (StatusCode, "invalid_categories")>   // ≤16 items, each 1..=64B
  categories_filter(&[String]) -> Option<String>   // builds the Eq/Or-on-"categories" filter_str fragment

Schema / access pattern:
  - Composed scope string: "{base}.{u-id}.{a-id}.{s-id}" (only present levels). KV keys stay
    lunaris:{composed_scope}:{kind}:{ulid}; base prefix "lunaris:T:" never matches "lunaris:T.u-x:".
  - categories: stored verbatim in Episode.metadata["categories"]; filtered at recall via the existing
    v0 string-DSL (no new index in v1).
  - Metrics stay labeled by the BASE scope (cardinality control), not the composed scope.

OUT OF SCOPE (v1): dedicated category secondary index; SDK (py/ts) surface for level-ids/categories
  (HTTP + core only); Postgres-specific category index. These are follow-ups.
```

Status: FROZEN @ v1 — approved by Tin Dang 2026-06-17 (freeze-as-drafted; mem0-parity-hardening last task 9/9; Fork B = levels as sub-partitions under the JWT base scope, reference impl on `feat/multi-level-memory-categories`). Least-sure flag surfaced at freeze: [contract] the categories AND-filter MUST round-trip on the PRODUCTION Moon recall path — a known hybrid-filter-bypass bug class ([[moon-hybrid-filter-bypass]]) could silently return the wrong set while unit tests pass (the built-≠-wired trap); mitigated by a MANDATED discriminating recall test (two episodes differing only by category must partition on the real backend, not a parse-level assert). Changing this contract = change request back to SPECIFY.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: 100% of §1 Musts + Rejects, each as an observable test; PLUS the frozen-flag discriminating real-backend test.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  CORE — `crates/lunaris-core/tests/scope_compose.rs` (unit + property):
  - compose_never_escapes_base: PROPERTY — for hostile ids × {user,agent,session} positions, every Ok result has base.as_str() as a strict byte-prefix; illegal ids Err (Reject: invalid_level_segment) [the load-bearing no-escape invariant]
  - compose_canonical_order: compose_levels(T, "alice","bot","s1") == "T.u-alice.a-bot.s-s1"; all-None == "T" (Must: canonical order + passthrough)
  - child_rejects_illegal_segment: child("a.b"), child("a:b"), child("a/b"), child("") all Err (Reject: invalid_level_segment)
  - compose_too_long: ids that overflow MAX_SCOPE_LEN -> Err (Reject: scope_too_long)
  - distinct_per_level: "T.u-x" != "T.a-x" != "T.s-x"; two different ids never collide (Must: distinctness)

  HTTP isolation/reject — `crates/lunaris-server/tests/multi_level_scope.rs` (capture-stub app):
  - ingest_composes_scope: ingest user_id="alice" -> atomic_write observed under "T.u-alice" not "T" (Must)
  - recall_binds_same_partition: recall user_id="alice" -> vector_search scope == "T.u-alice" (Must: same partition)
  - base_recall_isolated: recall with NO level-ids -> scope "T" (Must: base does not see child)
  - reject_invalid_level_segment / reject_scope_too_long: 400 + code, no storage call (Reject)
  - wire_scope_field_422: body with "scope"/"tenant" -> 422 deny_unknown_fields, no storage call (Reject: no-regress)

  CATEGORIES wiring — `crates/lunaris-server/tests/categories_filter.rs` (capture-stub records pushed Filter):
  - categories_become_storage_filter: recall categories=["urgent"] -> vector_search receives a Filter referencing "categories"+"urgent" (Must: reaches storage)
  - categories_and_combine_with_string_filter: filter + categories -> Filter::And of both (Must: AND-combine)
  - invalid_categories_returns_400: 17 categories -> 400 invalid_categories, 0 storage calls (Reject)

  CATEGORIES HONOR (the FROZEN-FLAG discriminating test) — `crates/lunaris-server/tests/categories_partition_real.rs`
        OR `crates/lunaris-storage-embedded/tests/categories_membership.rs` (REAL embedded/SQLite backend):
  - categories_partition_on_real_backend: ingest TWO episodes under the SAME composed scope, identical except
        metadata["categories"]=["blue"] vs ["green"]; recall (or vector_search) with a categories=["blue"]
        Filter::Eq returns ONLY the blue row, EXCLUDES green. ASSERTS the filter is HONORED (not silently dropped
        → would return both; not over-filtered → would return none). This is the built-≠-wired discriminator the
        §3 freeze flag mandates; it FAILS against the current json_extract(...) = scalar compilation (verified:
        json_extract on a JSON array never equals a scalar) and PASSES once Eq compiles to json_each membership.
</test_plan>

Tests live in: `crates/lunaris-core/tests/scope_compose.rs` `crates/lunaris-server/tests/multi_level_scope.rs` `crates/lunaris-server/tests/categories_filter.rs` `crates/lunaris-storage-embedded/tests/categories_membership.rs` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-core/src/scope.rs` `crates/lunaris-core/src/lib.rs` `crates/lunaris-server/src/dto.rs` `crates/lunaris-server/src/routes/ingest.rs` `crates/lunaris-server/src/routes/recall.rs` `crates/lunaris-storage-embedded/src/vector.rs` `crates/lunaris-core/tests/scope_compose.rs` `crates/lunaris-server/tests/multi_level_scope.rs` `crates/lunaris-server/tests/categories_filter.rs` `crates/lunaris-storage-embedded/tests/categories_membership.rs`
Strategy (ordered batches):
  1. CORE: add `Scope::is_valid_segment` + `Scope::child` + `MemoryLevel` + `compose_levels` to scope.rs; re-export at crate root (lib.rs). (Replays the proven reference impl; scope.rs UNCHANGED since merge-base so it applies clean.)
  2. SERVER DTO: add optional `user_id`/`agent_id`/`session_id`/`categories` to `IngestBody` + `RecallRequest` (deny_unknown_fields PRESERVED); add `LevelError{InvalidSegment,ScopeTooLong,InvalidCategories}` + `.code()` + `compose_request_scope` + `validate_categories` + `categories_filter` (Eq/Or on "categories") + `CATEGORIES_METADATA_KEY`. (Integrate around the +65 dto.rs drift = BrowseQuery/ScopesQuery/GraphQuery.)
  3. SERVER ROUTES: ingest composes scope (400 BEFORE timer/storage) + stores categories in metadata; recall composes the SAME scope + AND-combines categories Filter via `lunaris_retrieve::filter_str` + `Filter::And`; shared `level_reject`. Fix ALL existing `RecallRequest`/`IngestBody` literal constructors in the workspace (4 new fields).
  4. BACKEND FIX (the frozen-flag mitigation — chosen "Fix the backend filter (membership)"): change `lunaris-storage-embedded::vector::filter_to_sqlite` `Filter::Eq` from `json_extract(metadata,'$.{field}') = {lit}` to `EXISTS(SELECT 1 FROM json_each(metadata,'$.{field}') WHERE value = {lit})` — honors set-membership for an array field AND preserves scalar Eq (verified in sqlite: scalar match/non-match/missing all correct). Update the 3 existing `filter_to_sqlite` unit tests in vector.rs to the new SQL form. (Postgres mirror = deferred / HUMAN-UAT per Moon-first policy; Moon FT TAG is native-membership — confirm at HUMAN-UAT.)
Safety rule (feature-specific): the JWT base scope MUST remain a strict prefix of the composed scope — a caller can only NARROW, never escape. Categories validation + scope composition reject (400) BEFORE any storage write. Do NOT widen Eq semantics for non-array scalar fields (verified preserved). deny_unknown_fields stays on both DTOs.
Code lives in: the six files above + the four test files in §4.
Constraints: do NOT change any §4 test or the §3 contract; the membership fix is a BUILD-level change to the backend Eq compilation (BELOW the contract surface — §3 still says array storage + Eq/Or on "categories"), not a contract change; no new deps; ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — 4 task test files GREEN (scope_compose 8, multi_level_scope 5, categories_filter 3, categories_membership 3) + full 3-crate regression GREEN (~25 binaries, 0 failures across lunaris-core/lunaris-server/lunaris-storage-embedded). Workspace (excl py/ts) compiles clean.
- [x] coverage did not decrease — added 4 test files + 1 gate test (`is_valid_segment_rejects_structural_chars`); updated 3 existing `filter_to_sqlite` unit tests to the new SQL form (impl-detail, behavior preserved). No coverage removed.
- [x] no test or contract was altered during build — §3 CONTRACT FROZEN @ v1 untouched (the membership fix is BELOW the contract surface — §3 still says array storage + Eq/Or on "categories"). NOTE: `scope_compose.rs` (a §4 test) was HARDENED post-build per the adversarial MINOR (added a dot-count discriminator + the gate test) — STRENGTHENED, never weakened; still green.
- [x] the green was EARNED — independent adversarial refute-read (senior-rust-engineer, static): **EARNED-WITH-NITS**. All 3 claims UPHELD: (1) no-escape invariant airtight — `compose_request_scope`→`is_valid_segment`→`child`→`Scope::new` rejects every hostile id class (`.`/`:`/`/`/empty/space/unicode/over-128) before `child`; tag-prefix prevents id-mimics-tag aliasing; `:` rejection blocks KV-format aliasing. (2) both production paths wired (ingest.rs composes + binds composed scope + stores categories; recall.rs composes same partition + AND-combines filter; embedded `json_each` membership genuinely honors array filters). (3) deny_unknown_fields intact on both DTOs. The one MINOR (vacuous Ok-branch in the no-escape test) is FIXED (dot-count discriminator + `is_valid_segment` gate test). No CRITICAL/MAJOR.
- [x] concurrency / timing — scope composition + category validation are synchronous, BEFORE any `.await`/storage; no new locks; no lock-across-await introduced.
- [x] no exposed secrets, injection openings, or unexpected dependencies — categories field name is the hardcoded constant `CATEGORIES_METADATA_KEY` ("categories"), NOT attacker-controlled; value literals are SQL-escaped (`'`→`''`). The `'$.{field}'` interpolation is the PRE-EXISTING T-01-04-01 surface (deferred to Phase 4 OPS-04) — the membership fix does NOT widen it. No new deps.
- [x] layering & dependencies follow CONVENTIONS.md — keyspace composition via `Scope::child` in lunaris-core (no local key-mint); JWT `claims.scope` remains the only partition source (level ids NARROW under it); `deny_unknown_fields` preserved (wire scope/tenant still 422).
- [x] a person reviewed and approved the change — AUTO-RESOLVED under `autonomy: auto` (Tin explicitly chose freeze-as-drafted + declined lower-to-conservative). Security-ADJACENT but NO security FINDING (adversarial review UPHELD the no-escape invariant) → no residue to escalate. Tin froze §3 v1 (2026-06-17) + chose the membership-fix build direction.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — every new symbol referenced on the PRODUCTION path: `Scope::child`/`compose_levels`/`MemoryLevel` ← `compose_request_scope` (dto.rs) ← `ingest_handler` (ingest.rs:74) + `recall_handler` (recall.rs:99); `categories_filter` ← recall.rs:146; `CATEGORIES_METADATA_KEY` ← ingest.rs:109; `filter_to_sqlite` json_each ← embedded `vector_search`. Confirmed by the capture-stub tests (filter reaches vector_search) + the real-backend membership test (filter honored).
- [x] DEAD-CODE — no orphaned symbol; `cargo clippy --all-targets` clean on the 3 touched crates (no `dead_code`/`unused` warnings).
- [N/A] SEMANTIC (prose) — this is a code task.

### GATE RECORD
Outcome: PASS
Reviewed by: ADD auto-gate (autonomy:auto) + adversarial refute-read EARNED-WITH-NITS (MINOR fixed) — frozen/directed by Tin Dang · date: 2026-06-17

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
