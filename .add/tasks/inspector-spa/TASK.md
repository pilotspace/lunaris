# TASK: Read-only web dashboard SPA (browse · lineage · graph · disabled Phase-2 timeline)

slug: inspector-spa · created: 2026-06-16 · stage: production
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

A thin, read-only, self-contained vanilla-JS dashboard served by `lunaris-server`, consuming the four Phase-1 read endpoints already built this milestone. Tin chose (2026-06-16) **vanilla single-file, server-served**: one `inspector.html` embedded via `include_str!` + one public root route. NO Node toolchain, NO external CDN (offline-capable internal tool).

Touches (files · symbols · signatures):
- `crates/lunaris-server/src/lib.rs:251-261` — the ROOT `Router` (NOT the `/v1` nest). Add `.route("/", get(routes::ui::ui_handler))` alongside the already-public `/healthz` + `/metrics` (no `scoped_auth`): the SHELL is public; only the `/v1/*` calls it makes are recall-scoped.
- NEW `crates/lunaris-server/src/routes/ui.rs` — `ui_handler() -> impl IntoResponse` returning `axum::response::Html(include_str!("../../static/inspector.html"))` (asset embedded in the binary — no runtime file dependency).
- NEW `crates/lunaris-server/static/inspector.html` — the single-file SPA (HTML+CSS+vanilla JS).
- `crates/lunaris-server/src/routes/mod.rs` — `pub mod ui;`.
- The four endpoints the SPA consumes (DONE this milestone, frozen contracts): `GET /v1/scopes` (scope picker), `GET /v1/browse/{kind}?cursor=&limit=` (browse table + pagination), `GET /v1/detail/{kind}/{id}` (lineage drawer: primitive + provenance.source_episodes/confidence/entities), `GET /v1/graph?root=&depth=` (graph canvas: root + nodes).

Context (working folder): a new `static/` dir under the crate + a new `routes/ui.rs` + one root route. No DTO, no migration, no new crate dependency (`axum::response::Html` + `include_str!` are std/axum).

Honors (patterns / conventions): shell is PUBLIC at the root (mirrors `/healthz`+`/metrics` being open, NOT under `/v1`); the SPA holds the recall Bearer token ONLY from a user-entered field (persisted to `localStorage`), never hardcoded; **render all API data via `textContent` / `createElement` — NEVER `innerHTML` with response data** (stored-XSS guard: episode/fact text + entity names are agent-supplied); a single-file inline-script SPA needs a CSP that permits `'unsafe-inline'` for script/style but pins `connect-src 'self'` (documented tradeoff); entity/relation browse + the disabled timeline are honored from the frozen browse/detail decisions.

Anchors the contract cites: `ui_handler` + `axum::response::Html` + `include_str!`; the root `/` route (public); the four `/v1` endpoint paths the shell binds; the disabled Phase-2 timeline affordance; the textContent/CSP XSS guard.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: inspector-spa — single-file read-only Memory Inspector dashboard, server-served
Framings weighed: **vanilla single-file HTML embedded + served at `/`** (chosen, Tin 2026-06-16) · standalone `file://` HTML (rejected — no server route to integration-test, weakest TDD) · React/Vite framework SPA (rejected — first Node toolchain in a Rust monorepo, over-engineered for read-only Phase-1)
Must:
<must>
  - `GET /` → 200 `text/html` serving the self-contained Inspector shell. The route is PUBLIC (no Bearer required to load the page); it sits at the root alongside `/healthz`+`/metrics`, NOT under the `scoped_auth("recall")` `/v1` nest.
  - The shell is fully self-contained: inline CSS + inline vanilla JS, NO external network/CDN reference (offline-capable; embedded in the binary via `include_str!`).
  - The shell binds all four Phase-1 read endpoints (relative, same-origin): `GET /v1/scopes` (scope picker), `GET /v1/browse/{kind}?cursor=&limit=` (browse table + Next pagination via `next_cursor`), `GET /v1/detail/{kind}/{id}` (lineage drawer rendering `primitive` + `provenance.source_episodes`/`confidence`/`entities`), `GET /v1/graph?root=&depth=` (graph canvas rendering `root` + `nodes`).
  - A recall Bearer token is entered by the user in a token field, persisted to `localStorage`, and sent as `Authorization: Bearer <token>` on every `/v1` fetch. The token is NEVER hardcoded in the shell.
  - A timeline panel is present but DISABLED, labelled a Phase-2 affordance (no `/v1/history` call is wired).
  - All API-derived strings (episode/fact text, entity names, summaries) are rendered via `textContent`/DOM construction — NEVER `innerHTML` — so agent-supplied content cannot inject markup (stored-XSS guard).
  - entity/relation browse/detail responses (`graph_native: true`) route the user to the graph canvas rather than rendering an empty table/drawer as "no data".
Reject:
<reject>
  - (No request inputs to reject — `GET /` is parameterless and public.) The shell surfaces, but does not crash on, the API's own typed errors (401 missing/expired token → a "set your token" prompt; 501 graph_unavailable → a "graph not enabled" notice; 4xx/5xx → the `error` envelope shown inline).
</reject>
After:
<after>
  - A reviewer opens `http://<host>/`, pastes a recall token, picks a scope, and browses every primitive kind, opens a lineage drawer, and renders an entity neighborhood — without writing a query.
  - The dashboard issues only GET requests (strictly read-only); no write path is reachable from the UI.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ **A Rust integration test over the served shell is a sufficient ADD red/green for a browser SPA** — lowest confidence because it asserts the served *contract* (status, content-type, that the shell wires each endpoint path + the token field + the disabled-timeline affordance + no hardcoded token), NOT live DOM behavior in a browser (no headless runner in this Rust repo). If wrong: a rendering bug ships untested. Mitigation: the JS is deliberately minimal + reviewed for XSS-safety by hand at verify; the four endpoints it calls are each already contract-tested; the affordance/wiring markers are stable, behaviour-level asserts (not brittle layout strings).
  - [x] `GET /` is free at the root router (no existing `/` route) — confirmed at `lib.rs:251-261` (only `/healthz`, `/metrics`, and the `/v1` nest exist).
  - [x] A public (unauthenticated) shell is safe — confirmed: it contains no secret (token is user-entered at runtime) and every data-bearing call is still `scoped_auth("recall")`-gated server-side; mirrors the already-public `/healthz`+`/metrics`.
  - [x] `include_str!` embedding (not `ServeDir`) is the right delivery — confirmed: makes the published binary self-contained (no runtime asset path), avoids a `tower-http` `fs` feature add.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: the shell is served publicly as HTML
  Given a running server
  When GET / with NO Authorization header
  Then 200 with content-type text/html (the shell loads without a token)

Scenario: the shell wires all four read endpoints
  Given the served shell
  When I inspect its source
  Then it references /v1/scopes, /v1/browse/, /v1/detail/, and /v1/graph

Scenario: the shell carries a token field, not a hardcoded secret
  Given the served shell
  When I inspect its source
  Then it contains a token input and reads it dynamically (localStorage / the field), and contains NO hardcoded Bearer token literal

Scenario: the timeline is present but disabled (Phase 2)
  Given the served shell
  When I inspect its source
  Then it contains a timeline affordance marked disabled / Phase-2 and wires NO /v1/history call

Scenario: the shell is self-contained (no external CDN)
  Given the served shell
  When I inspect its source
  Then it has no external http(s):// script/style/src reference (offline-capable)

Scenario: API data is rendered XSS-safely
  Given the served shell
  When I inspect its rendering code
  Then it uses textContent / DOM construction for response data and never assigns innerHTML from a fetch result

Scenario: the dashboard is read-only
  Given the served shell
  When I inspect its fetch calls
  Then every request is a GET (no POST/PUT/DELETE/PATCH to any /v1 route)
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
GET /            (PUBLIC — root router, NOT under /v1; no Bearer required to load)
  200 -> text/html : the self-contained Inspector shell
         (Content-Type: text/html; charset=utf-8)
         (Content-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline';
          style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data')

Shell invariants (asserted on the served body):
  - references each of: "/v1/scopes", "/v1/browse/", "/v1/detail/", "/v1/graph"
  - contains a token input (id="token") + reads it dynamically ("localStorage")
  - contains NO hardcoded Bearer token literal
  - contains a disabled Phase-2 timeline affordance ("Phase 2" + a disabled control) + NO "/v1/history"
  - contains NO external "http://" / "https://" script|style|src reference (self-contained)
  - renders fetch data via "textContent" and never "innerHTML =" from a response
  - issues only GET to /v1 (no fetch with method POST/PUT/DELETE/PATCH)

Serving: ui_handler() -> axum::response::Html(include_str!("../../static/inspector.html")),
         registered at the ROOT Router `.route("/", get(...))`. No write. No new table/DTO/dep.
Consumes (frozen, already shipped): GET /v1/scopes · /v1/browse/{kind} · /v1/detail/{kind}/{id} · /v1/graph
```

Status: FROZEN @ v1 — approved by Tin Dang (fully-auto delegation + explicit approach choice, 2026-06-16)

Least-sure flag surfaced at freeze: [test] **a Rust integration test over the served shell substitutes for
browser DOM testing.** The suite asserts the served *contract* — status, content-type, that the shell wires
each endpoint + the token field + the disabled timeline + no hardcoded secret + XSS-safe rendering markers —
but cannot exercise live DOM behaviour (no headless browser in this Rust-only repo). Cost if wrong: a runtime
rendering/interaction bug ships untested. Mitigation: the JS is deliberately minimal and hand-reviewed for
XSS-safety at verify; each of the four endpoints it calls is independently contract-tested; the asserted
markers are behaviour-level (wiring + affordances + safety), not brittle layout strings. [contract] secondary:
the CSP permits `'unsafe-inline'` (a single-file inline-script SPA requires it) — accepted as a documented
tradeoff for a read-only internal tool, with `connect-src 'self'` pinning exfiltration and `textContent`-only
rendering closing the XSS vector that `'unsafe-inline'` would otherwise widen.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every §3 shell invariant (all 7 scenarios) hit by ≥1 test.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_ui_served_public_html: GET / with NO token → 200 + content-type text/html (shell loads tokenless)
  - test_ui_wires_all_four_endpoints: body contains "/v1/scopes","/v1/browse/","/v1/detail/","/v1/graph"
  - test_ui_token_field_no_hardcoded_secret: body has id="token" + "localStorage"; assert no hardcoded "Bearer "+token literal
  - test_ui_timeline_disabled_phase2: body contains a "Phase 2" timeline affordance + "disabled"; body does NOT contain "/v1/history"
  - test_ui_self_contained_no_cdn: body contains no "http://" or "https://" (no external CDN/src)
  - test_ui_xss_safe_rendering: body uses "textContent" and contains no DOM-HTML-sink assignment from a response
  - test_ui_read_only_only_get: body contains no `method:"POST"|PUT|DELETE|PATCH` fetch option (read-only)
</test_plan>

Tests live in: `crates/lunaris-server/tests/inspector_ui.rs` · MUST run red (no `/` route) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-server/static/inspector.html` `crates/lunaris-server/src/routes/ui.rs` `crates/lunaris-server/src/routes/mod.rs` `crates/lunaris-server/src/lib.rs` `crates/lunaris-server/tests/inspector_ui.rs`
Strategy (ordered batches): 1. `static/inspector.html` — the single-file SPA (inline CSS+JS; token field; scope picker; kind selector + browse table w/ Next; detail drawer; graph canvas; disabled timeline); 2. `routes/ui.rs` — `ui_handler` returning `Html(include_str!(...))` + CSP header; 3. `routes/mod.rs` += `pub mod ui;`; 4. `lib.rs` register `GET /` on the ROOT router (public, no scoped_auth).
Safety rule (feature-specific): render ALL fetch-derived strings via `textContent`/`createElement` (never a DOM HTML sink) — agent-supplied memory content is untrusted; CSP pins `connect-src 'self'`; the shell carries NO secret (token user-entered → localStorage); only GET requests.
Code lives in: `crates/lunaris-server/src/` + `crates/lunaris-server/static/`
Constraints: do NOT change any test or the contract; allow-list packages only (no new dep — `axum::response::Html` + `include_str!`); ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — `cargo test -p lunaris-server` = 137 passed / 3 ignored; `--test inspector_ui` = 7/7 green
- [x] coverage did not decrease — +7 new tests (inspector_ui), 0 removed; full crate suite still green
- [x] no test or contract was altered during build — §3 FROZEN @ v1 untouched; inspector_ui.rs unchanged since red run (build added src only)
- [x] the green was EARNED, not gamed — the 2 vacuous absence-only asserts caught in the red run were given positive anchors (`<html`, `fetch(`) so all 7 were red pre-build; served body == `include_str!` asset (no test-only fixture to overfit); grep on the served asset independently confirms each invariant (innerHTML=0, textContent=8, http(s)=0, mutation-verbs=0)
- [x] concurrency / timing of the risky operation is safe — `ui_handler` is a pure const-string responder: no storage touch, no lock, no `.await` on shared state, no I/O; nothing to race
- [x] no exposed secrets, injection openings, or unexpected dependencies — shell carries no Bearer JWT (token entered at runtime → localStorage); render path is `textContent`-only (no DOM HTML sink); ids escaped via `encodeURIComponent` (×4); CSP `connect-src 'self'` pins all fetch to origin, `base-uri 'none'`/`form-action 'none'`; no new crate deps (`include_str!` + axum `Html`)
- [x] layering & dependencies follow CONVENTIONS.md — public `/` is the ONLY unauthenticated data-free surface; every `/v1/*` call the shell makes stays `scoped_auth("recall")`-gated server-side (claims.scope is the only scope source); no DTO added (GET shell, no request body)
- [x] a person reviewed and approved the change — fully-auto milestone delegation (Tin Dang); no security finding, so no HARD-STOP escalation required

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `ui_handler` ← registered at root `/` in `lib.rs` (public, before `/healthz`); `INSPECTOR_HTML` ← `include_str!("../../static/inspector.html")` in `routes/ui.rs`; `routes::ui` ← `pub mod ui;` in `routes/mod.rs`; the route is exercised live by all 7 `inspector_ui` tests via `oneshot(GET /)`
- [x] DEAD-CODE (code) — `ui_handler`, `INSPECTOR_HTML`, `CSP` are all referenced (handler routed; consts used in the handler); clippy `-p lunaris-server --all-targets -D warnings` clean (no dead_code / unused warning)
- [x] SEMANTIC (prose / non-code) — read `static/inspector.html` in full: scope picker (GET /v1/scopes), kind+browse table with Next (GET /v1/browse/{kind}), detail drawer (GET /v1/detail/{kind}/{id}) rendering primitive+provenance, graph canvas as HTML node-cards (GET /v1/graph?root=&depth=), timeline panel present but `disabled` + labelled "Phase 2"; all data via `textContent`/`createElement`; confirmed no `innerHTML`, no `http(s)://`, no POST/PUT/DELETE/PATCH

### GATE RECORD
Outcome: PASS
Reviewed by: Tin Dang (fully-auto milestone delegation) · date: 2026-06-16

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
