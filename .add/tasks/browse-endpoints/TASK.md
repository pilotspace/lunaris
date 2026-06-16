# TASK: GET /v1/scopes + GET /v1/browse/{kind} (JWT-scoped, paginated)

slug: browse-endpoints · created: 2026-06-16 · stage: production
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

Touches (files · symbols · signatures):
- `crates/lunaris-server/src/lib.rs:190` — the `/episode/{id}` GET route block (rate_limit → tracing → `scoped_auth("recall")` layers, last `.route_layer` runs first). New GET routes `/scopes` + `/browse/{kind}` mirror it verbatim; read perm string = `"recall"`.
- `crates/lunaris-server/src/routes/episode.rs:44` — closest handler analog: `episode_handler(State(state): State<AppState>, Extension(claims): Extension<AuthClaims>, Path(id): Path<String>) -> Response`; `claims.scope` is the JWT `Scope`; returns `(StatusCode, Json).into_response()`; storage err → `map_error(LunarisError::Storage(e))`.
- `crates/lunaris-server/src/state.rs:39` — `AppState { lunaris: Arc<Lunaris>, tokens, runtime_flags }`. `crates/lunaris/src/handle.rs:823` `Lunaris::storage() -> Arc<dyn StoragePort>`; `:841` `clock()`; `:987` `Lunaris::list_scopes(prefix,limit,cursor) -> Result<ScopePage, LunarisError>`.
- `crates/lunaris-server/src/middleware/error.rs:24` — `map_error(LunarisError) -> Response`, envelope `{ "error": code, "message": msg }`; `Validate`→400, `Storage`→500, `NotSupported`→501.
- `crates/lunaris-server/src/dto.rs:33` — `#[serde(deny_unknown_fields)]` request-DTO convention (`IngestBody`/`RecallRequest`); NEW query DTO(s) live here.
- `lunaris_core::scan_page::<T>(&dyn StoragePort, &Scope, &[u8], Option<&str>, usize, Option<Hlc>) -> Result<Page<T>, ListError>` (read-api-pagination, FROZEN) + `lunaris_core::keyspace::{episode,chunk,entity,relation,fact,community}_prefix(&Scope)` — the per-kind scans.

Context (working folder): pure additive HTTP surface in `crates/lunaris-server/src/` — 2 routes + 1 handler module (`routes/browse.rs`) + 1–2 query DTOs. No new config/migrations.

Honors (patterns / conventions):
- Scope is the JWT `claims.scope` ONLY — `/browse` passes it to `scan_page`; route handlers ignore any wire-side scope (HTTP DTO discipline, CLAUDE.md).
- Public request DTOs carry `#[serde(deny_unknown_fields)]` (convention) — NB: axum `Query<T>` via serde_urlencoded does NOT enforce it at runtime (gotcha, documented), unlike JSON bodies.
- No lock across `.await`; reuse `map_error` envelope so error shape stays consistent.

Anchors the contract cites (the symbols §3 will name): `scan_page` + the 6 `*_prefix` helpers; `Lunaris::list_scopes` + `ScopePage`; `ListError::code()`; `AuthClaims.scope`; `map_error`. NEW surface §3 freezes: the two routes, the `{kind}` enum + `invalid_kind`, the `?cursor=&limit=` query DTO (default 20, cap = `MAX_PAGE`), the `{ items, next_cursor }` / `{ scopes, next_cursor }` envelopes, and the `ListError.code()`→HTTP-status map (invalid_*→400, corrupt_row/storage→500, list_scopes NotSupported→501).

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: two read-only, JWT-scoped HTTP endpoints — `GET /v1/scopes` (enumerate partitions) and `GET /v1/browse/{kind}` (paginate one scope's primitives of a kind, no query) — the dashboard's query-less browse surface over `scan_page` + `Lunaris::list_scopes`.
Framings weighed: per-kind path param `/browse/{kind}` dispatching to typed `scan_page::<T>` then serializing `Page<T>` to JSON (chosen — RESTful, mirrors `/episode/{id}`, matches the keyspace) · single `/browse?kind=` query param (rejected — less RESTful, same dispatch) · one `/memories` endpoint returning a tagged union of all kinds (rejected — heavier response, no per-kind paging).
Must:
<must>
  - `GET /v1/browse/{kind}` (kind ∈ episode|chunk|entity|relation|fact|community) with a valid `"recall"` token returns `200 { items: [<full primitive JSON>…], next_cursor }`, items scoped to `claims.scope`, ULID-ascending, ≤ `limit` (default 20). It dispatches kind → `{kind}_prefix(scope)` → `scan_page::<T>` and serializes the typed `Page<T>`.
  - Forward pagination: passing a returned `next_cursor` as `?cursor=` yields the next page (no skip/repeat — delegated to `scan_page`); the last page has `next_cursor: null`.
  - `GET /v1/scopes` with a valid token returns `200 { scopes, next_cursor }` from `Lunaris::list_scopes(prefix, limit, cursor)` — a partition ENUMERATION (deliberately NOT filtered by `claims.scope`).
  - Scope for `/browse` is `claims.scope` from the JWT ONLY; no wire-side `scope`/`tenant` is honored.
  - `as_of` is current state for Phase-1 — `/browse` calls `scan_page(…, None)`.
  - Both routes sit behind the existing `scoped_auth("recall")` + tracing + rate_limit layers (missing/invalid token → 401 via existing middleware, not new code).
</must>
Reject:
<reject>
  - `{kind}` not one of the six -> 400 "invalid_kind"
  - `limit == 0` -> 400 "invalid_limit"   (from scan_page)
  - `limit > MAX_PAGE` (500) -> 400 "limit_too_large"
  - malformed `cursor` -> 400 "invalid_cursor"
  - a stored value that fails to deserialize -> 500 "corrupt_row"
  - backend without scope enumeration (`/scopes` on Postgres) -> 501 "not_supported"
  - backend / storage error -> 500 "storage"
</reject>
After:
<after>
  - Response bodies match the frozen envelopes; the error envelope reuses `map_error`'s `{ "error": code, "message": msg }`.
  - No write occurs; `/browse` never returns a key outside `claims.scope`'s `{kind}` prefix.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ [x] DECIDED @ freeze v1 — `GET /v1/scopes` IS a CROSS-SCOPE enumeration (confirmed by Tin Dang). Fits the internal/local "we own the substrate" Phase-1 model + matches `Lunaris::list_scopes`. Residual (multi-tenant): revisit to filter by entitlement / operator role — an auth change, tracked for a later milestone.
  - [x] CONFIRMED — `/browse/{kind}` serializes the FULL stored primitive JSON (raw primitives, no trimmed DTO).
  - [x] CONFIRMED — default `limit` = 20, cap = `MAX_PAGE` (500).
  - [x] CONFIRMED — query-DTO `deny_unknown_fields` is convention-only; a smuggled `?scope=` is ignored (not rejected) — the wire-scope scenario asserts "ignored", which is the safe behavior.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
# ── Musts ──────────────────────────────────────────────────────────────
Scenario: Browse a kind returns a scoped, ordered page
  Given scope S holds 3 facts (u1<u2<u3) and a token bound to S
  When GET /v1/browse/fact?limit=2
  Then 200 with body { items: [<fact u1>, <fact u2>], next_cursor: "<ulid>" }
  And every item belongs to scope S, ULID-ascending, full primitive JSON

Scenario: Browse pagination walks all items once
  Given scope S holds 3 facts and a token bound to S
  When GET /v1/browse/fact?limit=2 then GET /v1/browse/fact?limit=2&cursor=<next_cursor>
  Then the two pages together are [u1,u2] then [u3], next_cursor null on page 2, none skipped/repeated

Scenario: Each of the six kinds is browsable
  Given scope S holds one of each primitive and a token bound to S
  When GET /v1/browse/{kind} for kind in episode|chunk|entity|relation|fact|community
  Then each returns 200 with that kind's item present

Scenario: Scopes enumeration lists partitions
  Given the backend holds scopes "a" and "b" and any valid token
  When GET /v1/scopes
  Then 200 with body { scopes: ["a","b"], next_cursor: null } (NOT filtered to the token's scope)

Scenario: Browse uses the JWT scope, not a wire scope
  Given a token bound to scope S and data in scope S and scope OTHER
  When GET /v1/browse/fact?scope=OTHER  (smuggled query param)
  Then 200 returns only scope-S facts (the ?scope= param is ignored)

# ── Rejects (each asserts what stays unchanged) ─────────────────────────
Scenario: Unknown kind is rejected
  Given a valid token
  When GET /v1/browse/widgets
  Then 400 { error: "invalid_kind" }
  And no scan is performed

Scenario: Zero limit is rejected
  Given a valid token
  When GET /v1/browse/fact?limit=0
  Then 400 { error: "invalid_limit" }
  And no items are returned

Scenario: Over-cap limit is rejected
  Given a valid token
  When GET /v1/browse/fact?limit=501
  Then 400 { error: "limit_too_large" }
  And no items are returned

Scenario: Malformed cursor is rejected
  Given a valid token
  When GET /v1/browse/fact?cursor=not-a-ulid
  Then 400 { error: "invalid_cursor" }
  And no items are returned

Scenario: A corrupt stored value surfaces as 500
  Given scope S has a fact-prefixed key whose value is not valid Fact JSON
  When GET /v1/browse/fact
  Then 500 { error: "corrupt_row" }
  And no partial page is returned as if complete

Scenario: Scopes enumeration on a non-enumerating backend is 501
  Given a backend whose list_scopes returns NotSupported (e.g. Postgres)
  When GET /v1/scopes
  Then 501 { error: "not_supported" }
  And no scope list is fabricated

Scenario: A backend/storage error surfaces as 500
  Given the storage scan errors mid-stream
  When GET /v1/browse/fact
  Then 500 { error: "storage" }
  And no partial page is returned

Scenario: Missing token is rejected by the auth layer
  Given no Authorization header
  When GET /v1/browse/fact
  Then 401 (the existing scoped_auth("recall") layer; handler not reached)
  And no scan is performed
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
GET /v1/scopes?prefix=&cursor=&limit=        auth: Bearer · scoped_auth("recall")
  200 -> { scopes: ["<scope-string>", …], next_cursor: string | null }
  501 -> { error: "not_supported", message }     # backend w/o enumeration (e.g. Postgres)
  500 -> { error: "storage", message }
  401 -> (no/invalid token — existing auth layer, not new code)
  source: Lunaris::list_scopes(prefix, limit, cursor); CROSS-SCOPE (NOT filtered by claims.scope)

GET /v1/browse/{kind}?cursor=&limit=         auth: Bearer · scoped_auth("recall")
  {kind} ∈ episode | chunk | entity | relation | fact | community
  scope := claims.scope (JWT only; wire-side scope ignored)
  200 -> { items: [<full primitive JSON of {kind}>, …], next_cursor: string | null }
  400 -> { error: "invalid_kind" | "invalid_limit" | "limit_too_large" | "invalid_cursor", message }
  500 -> { error: "corrupt_row" | "storage", message }
  401 -> (no/invalid token — existing auth layer)
  dispatch: kind → keyspace::{kind}_prefix(scope) → scan_page::<T>(storage, scope, prefix, cursor, limit, None) → serialize Page<T>

Query DTOs (dto.rs · #[serde(deny_unknown_fields)] convention; serde_urlencoded won't enforce it):
  BrowseQuery { cursor: Option<String>, limit: usize = 20 }                       // /browse/{kind}
  ScopesQuery { prefix: Option<String>, cursor: Option<String>, limit: usize = 20 } // /scopes
  limit cap = MAX_PAGE (500) — enforced by scan_page (browse) / passed to list_scopes (scopes)

Error map: invalid_kind (handler, pre-scan) + invalid_limit|limit_too_large|invalid_cursor → 400 ·
  corrupt_row|storage → 500 · list_scopes NotSupported → 501. Envelope = map_error's { error, message }.

Schema/access: READ-ONLY; no new tables/migrations; reuses scoped_auth("recall") + the map_error envelope.
```

Least-sure flag surfaced at freeze: [spec] `GET /v1/scopes` is a CROSS-SCOPE enumeration — any `"recall"` token learns every partition NAME (not its data). Accepted for the Phase-1 Moon-native local/single-tenant dashboard (matches `Lunaris::list_scopes`). Cost if wrong (multi-tenant): `/scopes` must filter to the caller's entitlements or gate behind an operator role — an auth change, not a handler tweak.

Status: FROZEN @ v1 — approved by Tin Dang (2026-06-16); /v1/scopes cross-scope confirmed. Changing this contract = a change request back to SPECIFY.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: 90%. Harness mirrors `crates/lunaris-server/tests/multi_agent_uat.rs`: `build_app()` → `axum::Router` over a `Lunaris::with_parts_keyword(...)` on embedded/in-memory storage; mint a `"recall"` token; seed primitives via ingest or `storage().atomic_write(scope, &[WriteOp::KvPut{ key: {kind}_key(scope,id), value }])`; drive with `app.oneshot(Request::get(uri).bearer(tok))` (tower `ServiceExt`); assert `(StatusCode, serde_json::Value)`. Storage doubles: a stub `StoragePort` whose `list_scopes` → `NotSupported` (for 501) and a fault-injecting `scan_range` (for storage-500).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_browse_fact_scoped_ordered_page: seed 3 facts in S / GET /v1/browse/fact?limit=2 / 200 items==[u1,u2] full JSON, scope==S, next_cursor present
  - test_browse_pagination_walks_once: seed 3 facts / page limit=2 then cursor / [u1,u2] then [u3], next_cursor null on p2, no dup/gap
  - test_each_kind_browsable: seed one of each / GET /v1/browse/{kind} for all six / each 200 with its item
  - test_scopes_enumeration: seed scopes a,b / GET /v1/scopes / 200 scopes⊇{a,b}, not filtered to token scope
  - test_browse_ignores_wire_scope: token bound to S, data in S and OTHER / GET /v1/browse/fact?scope=OTHER / 200 only S facts
  - test_invalid_kind_400: GET /v1/browse/widgets / 400 error=="invalid_kind", no scan
  - test_zero_limit_400: GET /v1/browse/fact?limit=0 / 400 error=="invalid_limit"
  - test_over_cap_limit_400: GET /v1/browse/fact?limit=501 / 400 error=="limit_too_large"
  - test_malformed_cursor_400: GET /v1/browse/fact?cursor=not-a-ulid / 400 error=="invalid_cursor"
  - test_corrupt_row_500: atomic_write a fact-prefixed key with non-Fact JSON / GET /v1/browse/fact / 500 error=="corrupt_row"
  - test_scopes_not_supported_501: app over stub storage (list_scopes→NotSupported) / GET /v1/scopes / 501 error=="not_supported"
  - test_storage_error_500: app over fault-injecting storage (scan_range errs) / GET /v1/browse/fact / 500 error=="storage"
  - test_missing_token_401: GET /v1/browse/fact with no Authorization / 401, handler not reached
</test_plan>

Tests live in: `crates/lunaris-server/tests/browse_endpoints.rs` · MUST run red (missing routes/handler) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-server/src/routes/browse.rs` (new — both handlers) · `crates/lunaris-server/src/routes.rs` (module decl — 2018-style file; or `routes/mod.rs` if that is the layout) · `crates/lunaris-server/src/routes/` (dir — covers the module file whichever form) · `crates/lunaris-server/src/lib.rs` (register the 2 routes) · `crates/lunaris-server/src/dto.rs` (BrowseQuery + ScopesQuery)
Strategy (ordered batches): 1. DTOs (BrowseQuery, ScopesQuery) in dto.rs 2. `routes/browse.rs`: kind→prefix dispatch, `scan_page::<T>` per kind, serialize Page<T>, inline ListError→status map; `scopes_handler` over `Lunaris::list_scopes` 3. wire both routes into lib.rs under the `scoped_auth("recall")` layer block 4. green the suite
Safety rule (feature-specific): scope is `claims.scope` ONLY — never read a wire-side scope; `scan_page` deref is `&*state.lunaris.storage()`; reuse `map_error` envelope shape; no lock across `.await`.
Code lives in: `crates/lunaris-server/src/`
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

- [x] all tests pass — `cargo test -p lunaris-server --test browse_endpoints` 13/13; full crate `cargo test -p lunaris-server` 102 passed / 3 ignored (no regression).
- [x] coverage did not decrease — 13 new tests (one per Must + per Reject) cover both new handlers + all branches; no production code path left untested.
- [x] no test or contract was altered during build — the 13 tests are byte-identical to the red suite. The frozen §3 BEHAVIOR (Must/scenario) is unchanged. The only deviation: §3's parenthetical "serde_urlencoded won't enforce `deny_unknown_fields`" is factually WRONG (it DOES reject unknown query params → 400). I corrected the MECHANISM (omitted the attribute from the two query DTOs) to PRESERVE the frozen behavior ("`?scope=` ignored → 200", Tin-confirmed). No test weakened; behavioral contract honored. Logged as a §7 SDD delta + surfaced to Tin.
- [x] the green was EARNED — manual adversarial refute-read (small additive surface; Rule 5). Two anti-overfit guards baked in: (1) `test_corrupt_row_500` seeds VALID JSON that is not a `Fact` — it fails ONLY because the handler deserializes into the typed primitive, not `serde_json::Value` (the obvious cheat); (2) the suite drives the real `lunaris_server::build()` router via `oneshot`, so 200s prove production route registration + auth-layer wiring, not direct handler calls. No vacuous asserts: every test pins id/scope/cursor/error-code, not just status.
- [x] concurrency / timing safe — `browse.rs` holds NO lock across `.await`: it calls `state.lunaris.storage()` (clones an `Arc`) then `scan_page(...).await`; no guard is held. No new shared mutable state. (CLAUDE.md lock-across-await invariant.)
- [x] no exposed secrets, injection openings, or unexpected dependencies — keyspace prefixes are built from the validated `Scope` via `lunaris_core::keyspace::*_prefix` (no string interpolation into a backend query); zero new crate deps; no secrets.
- [x] layering & dependencies follow CONVENTIONS.md — keyspace + `scan_page` imported from `lunaris-core` (not re-minted); `map_error` envelope reused; DTOs in `dto.rs`; JWT `claims.scope` is the only browse scope source.
- [ ] a person reviewed and approved the change — auto-gated under `autonomy: auto`; **flagged for Tin**: (a) the serde_urlencoded contract-note correction above, (b) `/v1/scopes` is CROSS-SCOPE (the frozen least-sure flag — any recall token enumerates partition NAMES).

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `browse_handler`/`scopes_handler` registered in `lib.rs` under `scoped_auth("recall")` (routes `/v1/scopes`, `/v1/browse/{kind}`); `routes::browse` declared in `routes/mod.rs`; `BrowseQuery`/`ScopesQuery`/`default_browse_limit` consumed by the handlers; `page_json`/`list_error_response` private helpers used by `browse_handler`. Confirmed live: the integration suite reaches all branches through the real router (404→200/400/500/501/401 transitions observed red→green).
- [x] DEAD-CODE (code) — no orphaned symbol; clippy `-p lunaris-server --all-targets -D warnings` clean (would flag dead/`unreachable_pub`; lib has `#![deny(unreachable_pub)]`).
- [ ] SEMANTIC (prose / non-code) — n/a (code change).

### GATE RECORD
Outcome: PASS (auto-resolved under `autonomy: auto` — complete evidence; no security/concurrency/architecture residue)
If RISK-ACCEPTED -> owner: — · ticket: — · expires: —   (never for a security gap)
Reviewed by: auto-gate (engine) — surfaced to Tin Dang for the 2 flags above · date: 2026-06-16

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): per-rejection rate by error code (`invalid_kind|invalid_limit|limit_too_large|invalid_cursor|corrupt_row|storage|not_supported`); `/v1/browse/{kind}` p50/p99 latency vs the 25ms recall contract; `/v1/scopes` call volume (cross-scope enumeration — watch for unexpected callers).
Spec delta for the next loop: `/v1/scopes` cross-scope enumeration is a single-tenant Phase-1 simplification; the moment a multi-tenant deployment lands, it must filter to the caller's entitlements or sit behind an operator role (auth change, tracked). Phase-2 timeline (`as_of`/superseded/forgotten) is the next milestone, gated on the history-source decision.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
- [SDD · open] A frozen contract note asserted "`serde_urlencoded` won't enforce `deny_unknown_fields`" — FALSE: axum `Query<T>` rejects unknown params with 400. The fix was to omit the attribute from read-only query DTOs (no overridable scope field → no smuggling vector) so the confirmed "ignore `?scope=`" behavior holds. Lesson: verify library-behavior claims before baking them into a contract; for `Query` DTOs, scope-safety comes from the handler reading `claims.scope` only, NOT from `deny_unknown_fields`. (evidence: `test_browse_ignores_wire_scope` went 400→200 after removing the attribute.)
- [ADD · open] The contract froze a security-relevant default (`/v1/scopes` cross-scope) behind a clearly-surfaced least-sure flag + explicit human confirm — the freeze flag did its job: the auto-gate accepted it as a signed design decision, not a silent risk. (evidence: §3 freeze note + §6 gate flag (b).)
