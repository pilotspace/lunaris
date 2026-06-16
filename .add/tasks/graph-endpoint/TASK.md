# TASK: GET /v1/graph?root=&depth= via graph_traverse

slug: graph-endpoint · created: 2026-06-16 · stage: production
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

Root-anchored entity-graph neighborhood for the SPA's graph canvas, served via `StoragePort::graph_traverse` (the operator-free path — I build the `CypherQuery` directly and return the raw `GraphResult` table, NOT scored recall Hits).

PROJECT-LEAD GROUND DECISION — **v1 returns the reachable NODE neighborhood, not edges.** `Graph::anchored`'s Legacy/Moon Cypher is `UNWIND $ids AS sid MATCH (n {id_hex: sid})-[*1..N]-(m) RETURN m.id_hex AS id, m.name AS name, m.type AS type LIMIT $k` — it yields reachable nodes `m`, and Moon's `CypherDialect::Legacy` (its parser rejects `MATCH p = …` outside `shortestPath`) cannot bind the per-hop edges of a variable-length path. So v1 returns `nodes` (root-anchored neighborhood within `depth`); explicit edge structure is a documented follow-up (needs `PathMetrics`/`Full` dialect or a dedicated bounded per-hop edge query). Phase-1 is Moon-native, so the Legacy template is the portable contract.

Touches (files · symbols · signatures):
- `crates/lunaris-core/src/storage/port.rs:66` — `graph_traverse(&self, scope: &Scope, query: &CypherQuery, as_of: Option<Hlc>) -> Result<GraphResult, StorageError>`; backends without graph → `Err(NotSupported)`.
- `crates/lunaris-core/src/storage/types.rs:151/300` — `CypherQuery { graph, cypher, params }` + `GraphResult { headers: Vec<String>, rows: Vec<Vec<Value>> }`. Canonical Legacy columns = `id` (dest entity ULID, 32-hex), `name`, `type`. Read columns BY HEADER NAME (wire-additive).
- `crates/lunaris-retrieve/src/operators/graph.rs` (re-exported at `lunaris_retrieve` root) — `LUNARIS_GRAPH_NAME="lunaris_graph"`, `MAX_GRAPH_HOPS=5`, `DEFAULT_GRAPH_HOPS=2`, `DEFAULT_GRAPH_K=30`, `EntityId`. The Legacy Cypher template (lines 277-285) I mirror; `hops` is a LITERAL in the cypher (openCypher requires literal bounds), `$ids` is a hex-string array param (injection-safe).
- `crates/lunaris-extract/src/types.rs:76/86` — `EntityId::from_hex(&str) -> Option<Self>` (validates exactly 32 ASCII hex) + `Display` (lowercase hex); the root id is the 32-char hex, never a ULID. `from_hex(...).map(|e| e.to_string())` normalizes case.
- `crates/lunaris-server/src/routes/recall.rs:55-69` — the graph gate to mirror: `if !caps.graph_native && !state.lunaris.graph_pipeline().is_enabled()` → 501 `graph_mode_unavailable`. `crates/lunaris/src/handle.rs:867` — `graph_pipeline() -> Arc<GraphPipelineHandle>` (`.is_enabled()`).
- `crates/lunaris-server/src/middleware/error.rs:24` — `map_error` for the 500/501 envelope; `crates/lunaris-server/tests/recall_graph_mode.rs:178/323` — the `graph_traverse` mock + `canned_graph_with` (`headers=[id,name,type]`, `id` cell = `format!("{}", EntityId)`) to base my test double on.

Context (working folder): new `crates/lunaris-server/src/routes/graph.rs` + `GraphQuery` DTO + 1 route in `lib.rs`. No migration. `lunaris-retrieve` + `lunaris-extract` already deps.

Honors (patterns / conventions): scope = JWT `claims.scope` ONLY (Moon scopes the graph via `graph_key(scope)` server-side; the `CypherQuery.graph` field value is the canonical `lunaris_graph`); query DTO carries NO `deny_unknown_fields` (serde_urlencoded enforces it and would reject future params — same call as `BrowseQuery`); **design-for-failure**: gate capability BEFORE touching storage (501), validate root/depth BEFORE the traversal (400), `NotSupported`/`Backend` from the traversal → `map_error`; no lock across `.await`.

Anchors the contract cites: `graph_traverse` + `CypherQuery`/`GraphResult`; the Legacy `MATCH (n {id_hex:sid})-[*1..depth]-(m)` template; `EntityId::from_hex`; `MAX_GRAPH_HOPS`/`DEFAULT_GRAPH_HOPS`/`DEFAULT_GRAPH_K`/`LUNARIS_GRAPH_NAME`; the `{ root, depth, nodes:[{id,name,type}], truncated, graph_native }` envelope; the capability gate (501).

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: graph-endpoint — root-anchored entity-graph neighborhood for the SPA graph canvas
Framings weighed: **raw neighborhood via `graph_traverse` (node table)** (chosen) · reuse `Graph::anchored` through the recall `RetrievalBuilder` (rejected — returns scored+hydrated Hits, not a node/edge table; couples the inspector to recall fusion/ranking) · full nodes+edges path query (rejected for v1 — Moon `Legacy` dialect can't bind variable-length path edges; deferred)
Must:
<must>
  - `GET /v1/graph?root=<32-hex>&depth=<n>` → 200 `{ root, depth, nodes: [{id,name,type}], truncated, graph_native: true }`, traversing the caller's scope graph from `root` out to `depth` hops via `storage.graph_traverse`.
  - The `CypherQuery` mirrors the proven `Graph::anchored` Legacy template: `UNWIND $ids AS sid MATCH (n {id_hex: sid})-[*1..{depth}]-(m) RETURN m.id_hex AS id, m.name AS name, m.type AS type LIMIT $k`, with `graph = LUNARIS_GRAPH_NAME`, `params.ids = [root_hex]`, `params.k = DEFAULT_GRAPH_K`. `depth` is spliced as a validated literal (openCypher requires literal path bounds); `root` rides ONLY in the `$ids` param (never interpolated into the cypher — injection-safe).
  - `nodes` is built by reading the `id`/`name`/`type` columns BY HEADER NAME (wire-additive); the result is deduped by `id` and EXCLUDES the root itself (the anchor is returned in `root`, and an undirected walk can revisit it).
  - `root` echoes the case-NORMALIZED lowercase hex (`EntityId::from_hex(root).to_string()`), and `depth` echoes the effective hop count (the default when the param is absent).
  - `truncated` (bool) = the traversal returned `≥ DEFAULT_GRAPH_K` raw rows (the `LIMIT` was hit — more neighbors may exist; honest, never silently capped).
  - `depth` defaults to `DEFAULT_GRAPH_HOPS` (2) when absent.
  - scope = JWT `claims.scope` ONLY; read-only (no `WriteOp`); `as_of = None` (Phase-1 current-state, matching browse).
Must NOT (v1 scope boundary):
  - return explicit edges between nodes (deferred — see §0 ground decision; documented follow-up).
Reject:
<reject>
  - `caps.graph_native == false` AND `graph_pipeline().is_enabled() == false` -> "graph_unavailable" (501, BEFORE any storage call — mirrors the recall gate)
  - `root` absent/empty/not exactly 32 ASCII hex (`EntityId::from_hex` → None) -> "invalid_root" (400, no traversal)
  - `depth == 0` or `depth > MAX_GRAPH_HOPS` (5) -> "invalid_depth" (400, no traversal) — the inspector REJECTS an out-of-range depth rather than silently clamping like the recall operator (an explicit contract beats a surprising clamp)
  - `graph_traverse` returns `Err(NotSupported)` -> 501 "not_supported" / `Err(_)` -> 500 "storage" (via `map_error`)
  - missing/invalid token -> 401 (the `scoped_auth("recall")` layer, before the handler)
</reject>
After:
<after>
  - The SPA can render a root-anchored entity neighborhood for any scope from one GET, with an honest `truncated` signal.
  - No write occurs anywhere (strictly read-only).
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ **Nodes-only (no edges) is acceptable for the Phase-1 graph canvas** — lowest confidence because the SPA (`inspector-spa`) graph canvas may want explicit edges to draw links, and a node cloud is a weaker "understand the graph" affordance. If wrong: `inspector-spa` needs an edge source → a v2 endpoint (or a `PathMetrics`-dialect / per-hop edge query). Mitigation: this is the portable Moon-`Legacy` contract today; the envelope is additive (an `edges` key can be added without breaking `nodes` readers); the gap is documented here + in the freeze flag.
  - [x] The `Legacy` `MATCH (n {id_hex:sid})-[*1..N]-(m)` template runs unchanged on every Phase-1 backend (Moon native; SQLite/embedded report `graph_native=false` → 501 before traversal) — confirmed: it is the exact template `Graph::anchored` ships against Moon, and the param-ref inline filter is NOT subject to the literal-value inline-filter bug.
  - [x] `root` is the 32-char EntityId hex, not a ULID — confirmed at `graph.rs:313` (`format!("{}", id)` into `$ids`) + `EntityId::from_hex` validates 32 hex.
  - [x] Rejecting (not clamping) an out-of-range `depth` is the right inspector contract — deliberate divergence from the operator's clamp; an inspector user typing `depth=9` should be told `invalid_depth`, not silently served `depth=5`.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: neighborhood traversal builds the right Cypher and returns nodes (DISCRIMINATING)
  Given a graph-native backend with a canned GraphResult of neighbor nodes
  When GET /v1/graph?root=<hex>&depth=2
  Then 200 with graph_native:true, depth:2, root:<normalized hex>, and nodes=[{id,name,type},...]
  And the recorded CypherQuery has cypher containing "[*1..2]" and "id_hex", params.ids==[<hex>], params.k present, graph=="lunaris_graph"

Scenario: depth defaults to DEFAULT_GRAPH_HOPS when absent
  Given a graph-native backend
  When GET /v1/graph?root=<hex>   (no depth)
  Then 200 with depth:2 and the recorded cypher contains "[*1..2]"

Scenario: root case is normalized and the anchor is excluded + neighbors deduped
  Given a canned GraphResult that includes the root id itself and a duplicate neighbor id
  When GET /v1/graph?root=<UPPER-hex>&depth=1
  Then 200 with root echoed as lowercase hex, nodes excludes the root id, and the duplicate neighbor appears once

Scenario: truncation is signalled when the LIMIT is hit
  Given a canned GraphResult with DEFAULT_GRAPH_K (30) rows
  When GET /v1/graph?root=<hex>&depth=2
  Then 200 with truncated:true

Scenario: graph unavailable backend is rejected before any traversal
  Given a backend with graph_native=false and the graph pipeline disabled
  When GET /v1/graph?root=<hex>&depth=2
  Then 501 graph_unavailable
  And graph_traverse was never called

Scenario: malformed root is rejected before any traversal
  When GET /v1/graph?root=not-hex&depth=2
  Then 400 invalid_root
  And graph_traverse was never called

Scenario: missing root is rejected
  When GET /v1/graph?depth=2   (no root)
  Then 400 invalid_root
  And graph_traverse was never called

Scenario: zero depth is rejected before any traversal
  When GET /v1/graph?root=<hex>&depth=0
  Then 400 invalid_depth
  And graph_traverse was never called

Scenario: over-cap depth is rejected before any traversal
  When GET /v1/graph?root=<hex>&depth=6
  Then 400 invalid_depth
  And graph_traverse was never called

Scenario: a backend traversal error is a 500
  Given a graph-native backend whose graph_traverse returns Err
  When GET /v1/graph?root=<hex>&depth=2
  Then 500 storage
  And no nodes are returned

Scenario: missing token is rejected by the auth layer
  When GET /v1/graph?root=<hex>&depth=2 with no Authorization header
  Then 401
  And graph_traverse was never called
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
GET /v1/graph?root=<32-hex>&depth=<n>   auth: scoped_auth("recall"); scope = JWT claims.scope ONLY; no body
                                        depth optional (default DEFAULT_GRAPH_HOPS=2)

  200 -> {
    root:  "<lowercase-32-hex>",          # case-normalized anchor id
    depth: <n>,                            # effective hop count (1..=5)
    nodes: [ { id: "<32-hex>", name: <str|null>, type: <str|null> }, ... ],   # deduped, root excluded
    truncated: <bool>,                     # true iff >= DEFAULT_GRAPH_K (30) raw rows (LIMIT hit)
    graph_native: true
  }
  501 -> { error: "graph_unavailable" }    # !caps.graph_native && !graph_pipeline().is_enabled() (pre-traversal)
  400 -> { error: "invalid_root" }         # root absent/empty/not exactly 32 ASCII hex
  400 -> { error: "invalid_depth" }        # depth == 0 or depth > MAX_GRAPH_HOPS (5)
  501 -> { error: "not_supported" }        # graph_traverse → Err(NotSupported) (defensive; gate usually catches first)
  500 -> { error: "storage" }              # graph_traverse → Err(_)
  401 -> (auth layer)                      # missing/invalid token, before the handler

Resolution order: (1) capability gate → 501 graph_unavailable ; (2) parse+validate root → 400 invalid_root ;
                  (3) validate depth (default 2; reject 0 or >5) → 400 invalid_depth ;
                  (4) graph_traverse(scope, query, None) → map rows | NotSupported=501 | Err=500.
Cypher (Legacy, mirrors Graph::anchored): graph=LUNARIS_GRAPH_NAME ("lunaris_graph");
  cypher="UNWIND $ids AS sid MATCH (n {id_hex: sid})-[*1..{depth}]-(m) RETURN m.id_hex AS id, m.name AS name, m.type AS type LIMIT $k";
  params={ ids: [root_hex], k: DEFAULT_GRAPH_K }. depth = validated literal; root only in $ids. No write. No new table/DTO migration.
```

Status: FROZEN @ v1 — approved by Tin Dang (fully-auto delegation, 2026-06-16)

Least-sure flag surfaced at freeze: [contract] **nodes-only — no explicit edges in v1.** The `inspector-spa`
graph canvas (task 5) may want edges to draw links between neighbors; v1 returns a root-anchored node
neighborhood only, because Moon's `Legacy` dialect cannot bind the per-hop edges of a variable-length path
(`MATCH p = …` is rejected; Phase-1 is Moon-native). Cost if wrong: task 5 needs an edge source → a v2
endpoint or a `PathMetrics`-dialect / bounded per-hop edge query. Mitigation: the envelope is additive (an
`edges` key can land later without breaking `nodes` readers); the contract is the portable substrate-truth
today; the discriminating test pins the exact `CypherQuery` the handler builds against the real
`graph_traverse` port, so the traversal shape can't silently drift.

### v2 amendment — 2026-06-16 (Memory Inspector live UAT; change-request by Tin)

Live UAT against real Moon surfaced two changes; the response ENVELOPE is unchanged
(additive), so this amends behaviour, not shape:

1. **Anchor correctness (bug fix).** The v1 Cypher `MATCH (n {id_hex: sid})-[*1..N]-(m)`
   used an inline-property filter that Moon SILENTLY IGNORES — it matched every node as an
   anchor, so neither `root` nor `depth` constrained the traversal (live-confirmed:
   `root=Bob&depth=1` returned a 2-hop node). The anchor now rides a `WHERE` clause:
   `UNWIND $ids AS sid MATCH (n)-[*1..{depth}]-(m) WHERE n.id_hex = sid RETURN …`. The same
   inline-filter bug was live in `lunaris-retrieve::Graph::anchored` (all 3 dialects) and is
   fixed there too. New live-Moon regression test:
   `lunaris-storage-moon/tests/graph_anchor_constrains.rs` (the mock suite records the
   CypherQuery string but never executes it, so it could not catch this).
2. **Empty root → all nodes (new behaviour).** An absent or empty/whitespace `root` is no
   longer `400 invalid_root` — it lists every node in the scope graph
   (`MATCH (n) … LIMIT $k`, no anchor, `depth` ignored), with `root:null` / `depth:null` in
   the response. Rationale: entity ids are graph-native (not browsable via `/v1/browse`), so
   without this a reviewer has no entry point. A non-empty `root` that is not 32-hex is still
   `400 invalid_root`.

<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every §3 resolution branch (all 11 scenarios) hit by ≥1 test.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_graph_neighborhood_and_cypher (DISCRIMINATING): graph-native mock + canned nodes → 200; assert nodes + the RECORDED CypherQuery (cypher has "[*1..2]"+"id_hex", params.ids==[hex], k present, graph=="lunaris_graph")
  - test_graph_default_depth: no depth → depth:2 + recorded cypher "[*1..2]"
  - test_graph_root_normalized_dedup_excludes_root: canned result incl root id + a dup neighbor; UPPER-case root → root echoed lowercase, nodes excludes root, dup once
  - test_graph_truncated_flag: canned 30 rows → truncated:true
  - test_graph_unavailable_501: graph_native=false (pipeline off) → 501 graph_unavailable, graph_traverse NOT called
  - test_graph_invalid_root_400: root=not-hex → 400 invalid_root, not called
  - test_graph_missing_root_400: no root → 400 invalid_root, not called
  - test_graph_zero_depth_400: depth=0 → 400 invalid_depth, not called
  - test_graph_over_cap_depth_400: depth=6 → 400 invalid_depth, not called
  - test_graph_storage_error_500: graph_native=true + graph_traverse Err → 500 storage
  - test_graph_missing_token_401: no token → 401, not called
</test_plan>

Tests live in: `crates/lunaris-server/tests/graph_endpoint.rs` · MUST run red (no route/handler) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-server/src/routes/graph.rs` `crates/lunaris-server/src/routes/mod.rs` `crates/lunaris-server/src/lib.rs` `crates/lunaris-server/src/dto.rs` `crates/lunaris-server/tests/graph_endpoint.rs`
Strategy (ordered batches): 1. `dto.rs` += `GraphQuery { root: Option<String>, depth: Option<usize> }` (no deny_unknown_fields); 2. new `routes/graph.rs` with `graph_handler(State, Extension<AuthClaims>, Query<GraphQuery>)` — gate → validate root → validate depth → build CypherQuery → graph_traverse → map rows (dedup, exclude root, header-name lookup); 3. `routes/mod.rs` += `pub mod graph;`; 4. `lib.rs` register `GET /v1/graph` mirroring the `/browse/{kind}` block.
Safety rule (feature-specific): capability gate (501) BEFORE any storage call; root/depth validation (400) BEFORE the traversal; `depth` only ever a validated literal (1..=5) in the cypher, `root` only in `$ids` (no string interpolation of user input into the query). Read-only — no `WriteOp`.
Code lives in: `crates/lunaris-server/src/`
Constraints: do NOT change any test or the contract; allow-list packages only (no new external dep — `lunaris-retrieve`/`lunaris-extract` already present); ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — `cargo test -p lunaris-server` = 130 passed / 3 ignored (13 suites); `graph_endpoint` 11/11.
- [x] coverage did not decrease — +1 suite (11 new tests), all green; no existing test removed.
- [x] no test or contract was altered during build — the only post-red edit to the test file was `cargo fmt` (line wraps), no assertion changed; §3 contract unchanged.
- [x] the green was EARNED, not gamed — adversarial refute-read (self): the discriminating test asserts the RECORDED `CypherQuery` (graph=="lunaris_graph", `[*1..2]` literal, `id_hex`, `params.ids==[root_hex]`, `k` present) — a stub returning canned data without building the query can't match the test's own root hex; the gate test asserts `graph_traverse` was NEVER called (kills a read-then-gate impl); dedup/exclude-root and `truncated` each kill a naive passthrough. No vacuous asserts, no fixture overfit.
- [x] concurrency / timing safe — no lock held across `.await`; the handler holds no storage guard; a single `graph_traverse` await, no shared mutable state.
- [x] no exposed secrets, injection openings, or unexpected dependencies — `root` rides ONLY in the `$ids` param (never interpolated into the cypher); `depth` enters the cypher ONLY as a validated literal (`1..=MAX_GRAPH_HOPS`); read-only (no `WriteOp`); no new crate dependency (`lunaris-retrieve`/`lunaris-extract` already present).
- [x] layering & dependencies follow CONVENTIONS.md — scope = `claims.scope` ONLY (Moon scopes the graph server-side via `graph_key(scope)`); reuses the canonical `LUNARIS_GRAPH_NAME`/`MAX_GRAPH_HOPS`/`DEFAULT_GRAPH_HOPS`/`DEFAULT_GRAPH_K` consts + `EntityId::from_hex` from `lunaris-retrieve`/`lunaris-extract` (no magic numbers, no local id-mint); capability gate mirrors `recall_handler`.
- [x] a person reviewed and approved the change — fully-auto delegation (Tin Dang, 2026-06-16); auto-PASS on complete evidence per `autonomy: auto`. NO security finding (cypher injection closed by param-only root + literal-only depth, scope isolation via claims.scope), so no HARD-STOP escalation.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `graph_handler` is referenced by the `/graph` route in `lib.rs`; `map_nodes` is called by `graph_handler`; `GraphQuery` is consumed by the handler; `routes::graph` is declared in `routes/mod.rs` and used in `lib.rs`. The discriminating test drives the registered route → real `graph_traverse` port and asserts the built query.
- [x] DEAD-CODE (code) — no new unused/orphaned symbol; `cargo clippy -p lunaris-server --all-targets -D warnings` = clean.
- [N/A] SEMANTIC (prose / non-code) — this is a code task.

### GATE RECORD
Outcome: PASS
If RISK-ACCEPTED -> owner: <name> · ticket: <link> · expires: <date>   (never for a security gap)
Reviewed by: Tin Dang (fully-auto delegation) · date: 2026-06-16

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): graph-mode 200-rate vs 501; per-rejection rate (invalid_root/invalid_depth); empty-vs-anchored ratio.
Spec delta for the next loop: live UAT (2026-06-16) proved `root`+`depth` did NOT constrain on Moon — the inline-property anchor filter `(n {id_hex: sid})` is silently ignored (matched every node). Fixed to the `WHERE n.id_hex = sid` form here + in `lunaris-retrieve::Graph::anchored`. Added empty-root → all-nodes. Open follow-up: explicit edges (v1 nodes-only) still deferred; the recall graph-mode shared the same anchor bug and is now fixed but lacks its own live-Moon assertion.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
- [TDD · open] A mock that records the `CypherQuery` STRING cannot catch a backend that mis-executes that string — the inline-filter bug passed graph-endpoint's gate and only surfaced in live UAT. Graph/Cypher contracts need a live-Moon discriminating test on the production path, not just a string-shape assertion (evidence: `graph_anchor_constrains.rs` is RED-worthy where the mock suite was green). Reinforces [[feedback_built_not_wired]].
- [SDD · open] "root-anchored neighborhood" was under-specified as a string template, not a behaviour — the freeze pinned the Cypher TEXT but not the observable "depth=1 excludes 2-hop nodes" property. Specify graph contracts by observable reachability, not query syntax (evidence: v1 froze the exact broken cypher and the gate passed).
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
