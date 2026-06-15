# TASK: LangGraph/CrewAI adapters + SDK DX; make the adapter claim true

slug: sdk-integrations-dx · created: 2026-06-15 · stage: production
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
- ADAPTER CODE TODAY = ZERO. Repo-wide grep for langgraph/crewai/letta finds only a conformance corpus string (`crates/lunaris-conformance/tests/sdk_embedder_parity.rs:157` = `"langgraph crewai letta second wave"`) + docs/planning prose. No stub trait, module, or example exists.
- `lunaris-server` MemoryProtocol HTTP verbs (blueprint §5.4) — the integration substrate an adapter talks to: `POST /v1/ingest` (`IngestBody{source,content,t_ref?,metadata}`, JWT-bound scope), `POST /v1/recall` (`RecallRequest{query,k,as_of?,filter?,mode}`), `POST /v1/forget`, `GET /snapshot`. (`crates/lunaris-server/src/routes/{ingest,recall}.rs`, `dto.rs`.)
- SDK surface (complete for core verbs): `crates/lunaris-py/python/lunaris/__init__.py` + `crates/lunaris-ts/index.d.ts` — `open(url)`, `Lunaris.{ingest,recall,forget,snapshot}`, `Scope`, `EpisodeBuilder`, `ScopedLunaris.{ingest,recall,dsl}`, DSL `Vector/Keyword/Graph` + `RetrievalBuilder`. DX debt: both quickstarts pass a raw dict to `ingest()` ("typed EpisodeBuilder lands in v0.3") and the TS `RetrievalBuilder.execute()` is split into a free `recallSimpleExecute` fn.
- Target interfaces (map cleanly, no new Lunaris primitive needed): LangGraph `BaseStore.{aput(ns,k,v),aget,asearch(ns,q)}` → ingest/recall scoped by namespace; CrewAI `Memory.{save(v,meta),search(q),reset()}`; Letta = service-shaped (point its backend URL at lunaris-server).
Context (working folder): `docs/competitive/mem0-gap-analysis.md` — §A row 8 + §C `sdk-dx` + §E `sdk-integrations-dx` (P1, effort **M**): adapters "promised in docs yet absent in code"; acceptance evidence = **"a LangGraph adapter example runs against lunaris-server; POSITIONING.md adapter claim matches shipped code."** §F flags `docs/POSITIONING.md` claim as "corrected" target.
Honors (patterns / conventions): MemoryProtocol-not-adapters (blueprint §5.4) + the MCP-server-as-universal-shim pivot (`.planning/architect/REALITY-CHECK.md:475`, shipped as `lunaris-mcp` 11 tools); built≠wired ([[feedback_built_not_wired]]) — a real adapter example must actually round-trip against the production server path, not a mock; py/ts SDK test caveat ([[feedback_py_ts_sdk_testing]]) — excluded from `cargo test --workspace`; Moon-first/defer-live-to-UAT ([[feedback_moon_first_pg_deferred]]).
Claim sites to reconcile: `docs/POSITIONING.md:105-107` + `docs/book/src/getting-started/why-lunaris.md:106-108` (ALREADY softened to "roadmap / not yet shipped at v0.7"); `docs/MIGRATING-FROM-ZEP.md:235` + `docs/book/src/migrating/zep.md:240` (STILL stale "v0.4 ecosystem milestone tracks this"); `CLAUDE.md:8` (accurate "second-wave"). NOTE: doc reconciliation overlaps the separate `mem0-docs-reconcile` task — coordinate to avoid double-editing.
Anchors the contract cites: the MemoryProtocol HTTP verbs (`/v1/ingest`, `/v1/recall`), the LangGraph `BaseStore` method shape, the adapter example location, and the doc claim lines that must match shipped reality.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Ship real, configurable memory adapters for **LangGraph + CrewAI + Letta** on top of a shared `LunarisClient` (HTTP MemoryProtocol + in-process lunaris-py SDK), each with a runnable example + a pytest suite, and reconcile the remaining "v0.4 ecosystem" doc overclaim so the adapter claim matches shipped code.
Framings weighed: **shared `LunarisClient` + 3 thin framework adapters (chosen — user: all three, both transports)** · per-framework standalone packages each re-implementing the client (rejected — duplicated transport logic, drift) · docs-only recipe (rejected by user — leaves the "adapter example runs" bar unmet).
Must:
<must>
  - `LunarisClient` (Python Protocol) with TWO impls, both scope-bound at construction and exposing `ingest(source, content, metadata) -> lsn` + `recall(query, k) -> list[Hit]`:
    · `HttpLunarisClient` — httpx → `POST /v1/ingest` + `POST /v1/recall` on lunaris-server, Bearer-JWT (scope from the token, never the wire — honors the server's scope discipline).
    · `SdkLunarisClient` — wraps the in-process lunaris-py handle (`open(url).scoped(scope)`).
  - LangGraph `LunarisStore(BaseStore)`: `aput(ns,key,value)`→ingest scoped by `ns`; `aget(ns,key)`; `asearch(ns,query)`→recall top-k (+ sync `put/get/search` if the BaseStore ABC requires them).
  - CrewAI `LunarisCrewAIStorage` (Memory storage interface): `save(value, metadata)`→ingest; `search(query, limit, score_threshold)`→recall; `reset()`→forget/scope-clear.
  - Letta connector: map Letta's archival-memory `insert`/`search` onto `LunarisClient`; if Letta's interface can't be cleanly subclassed at its current version, ship a thin connector + a documented config recipe pointing Letta at lunaris-server (see flag).
  - Every adapter is transport-agnostic — constructed with ANY `LunarisClient` (HTTP or SDK); the framework deps (langgraph/crewai/letta) are OPTIONAL extras, never hard deps of lunaris-py.
  - One runnable example per framework + a pytest suite exercising each adapter's method-mapping against a STUB `LunarisClient` (records ingest/recall calls) — no live model/Moon needed for the unit layer.
  - Doc reconciliation: update the stale "v0.4 ecosystem" claims (`MIGRATING-FROM-ZEP.md` + book mirror) and lift POSITIONING/why-lunaris from "roadmap" to "shipped" for exactly what ships — coordinated with the `mem0-docs-reconcile` task to avoid double-editing.
</must>
Reject:
<reject>
  - framework API drift (e.g. LangGraph `BaseStore` signature changed) -> adapter import/version-guards and raises a typed `UnsupportedFrameworkVersion` -> never a silent mis-map.
  - missing transport config (HTTP impl with no base URL / SDK impl with no wheel-backed handle) -> clear error AT CONSTRUCTION, not at first call.
  - a namespace/scope violating the Lunaris alphabet (`[A-Za-z0-9_\-.]{1,128}`, `:` rejected) -> rejected at the adapter boundary via `Scope.new` -> "invalid_scope" (no byte-aliasing across scopes).
</reject>
After:
<after>
  - The three adapters import + run; a LangGraph example round-trips against lunaris-server; pytest green against the stub client; the in-process SDK transport's live path is exercised by an example (live run = UAT per the lunaris-py wheel + backend caveat).
  - Every shipped-adapter doc claim matches code; no remaining "v0.4 ecosystem (shipped)" overclaim.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ [spec/contract] **Letta is the riskiest of the three** — its archival-memory / `StorageConnector` interface is heavier and less version-stable than LangGraph `BaseStore` / CrewAI `Memory`; a clean drop-in subclass may not exist at the pinned version. If wrong: the Letta deliverable degrades to a thin connector + a documented recipe (not a full class) → "all three" is partially met for Letta. Mitigation: gate Letta behind the same `LunarisClient` so the mapping is trivial once the connector seam is known; pin the Letta version in the example extra.
  ⚠ [spec] **Sizing** — 3 frameworks × 2 transports is L/XL for one ADD task; risk of an oversized task hard to verify in one gate. Mitigation: the shared `LunarisClient` keeps each adapter ~60–100 LOC; staged build (client → LangGraph → CrewAI → Letta → docs); the unit layer tests against a stub client so the suite stays fast + backend-free. If it proves too big at the freeze, split into a sub-milestone (client task + per-framework tasks).
  - [ ] [spec] exact current API shapes of LangGraph `BaseStore` / CrewAI `Memory` / Letta connector — pin against the installed framework versions during build; adapters version-guard + degrade.
  - [ ] [test] Python harness — pytest against a stub `LunarisClient`/server (red/green, no backend); the in-process SDK transport's LIVE test is UAT (needs the maturin wheel + a backend) per [[feedback_py_ts_sdk_testing]].
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: HttpLunarisClient round-trips the MemoryProtocol verbs
  Given a stub HTTP server recording POSTs to /v1/ingest and /v1/recall
  And an HttpLunarisClient bound to scope "agent_a" with a Bearer token
  When ingest(source, content, metadata) then recall(query, k=5) are called
  Then /v1/ingest received {source, content, metadata} (NO scope/tenant on the wire)
  And /v1/recall received {query, k:5} and the client returned the parsed hits

Scenario: the SAME adapter is transport-agnostic
  Given a LunarisStore constructed once with HttpLunarisClient and once with SdkLunarisClient
  When aput/asearch run against each
  Then both route through the client's ingest/recall with identical call shapes
  And the adapter code path does not branch on transport

Scenario: LangGraph LunarisStore maps store ops to ingest/recall
  Given a LunarisStore over a stub LunarisClient
  When aput(("u","mem"), "k1", {"text":"Alice joined Acme"}) then asearch(("u","mem"), "where did Alice join")
  Then the stub recorded one ingest scoped by namespace ("u.mem") carrying the value
  And asearch returned the recalled items mapped to LangGraph SearchItem shape

Scenario: CrewAI LunarisCrewAIStorage maps save/search/reset
  Given a LunarisCrewAIStorage over a stub LunarisClient
  When save("Alice joined Acme", {"agent":"a"}) then search("Acme", limit=3) then reset()
  Then save → one ingest, search → one recall(k=3) returning CrewAI result dicts
  And reset → a forget/scope-clear call (recorded), leaving other scopes untouched

Scenario: Letta connector maps insert/search (or degrades to a documented recipe)
  Given a LunarisLettaConnector over a stub LunarisClient (at the pinned Letta version)
  When insert(passage) then search(query) run
  Then insert → ingest and search → recall on the client
  And IF the pinned Letta interface cannot be subclassed cleanly, the connector ships
      as a thin shim PLUS a recipe doc, and the test asserts the shim's ingest/recall mapping

Scenario: framework API drift is rejected, not mis-mapped
  Given an installed framework whose adapter base class/signature does not match the pinned shape
  When the adapter module is imported / instantiated
  Then it raises UnsupportedFrameworkVersion
  And no partial/incorrect mapping is silently constructed

Scenario: missing transport config fails at construction
  Given an HttpLunarisClient with no base URL (or an SdkLunarisClient with no handle)
  When it is constructed
  Then construction raises a clear configuration error
  And no request is attempted at first use

Scenario: an invalid namespace/scope is rejected at the adapter boundary
  Given a namespace that maps to a scope containing ":" (or >128 chars / bad char)
  When an adapter op is invoked with it
  Then Scope.new rejects it → "invalid_scope" before any ingest/recall
  And no key is minted that could byte-alias another scope's partition

Scenario: shipped-adapter docs match reality
  Given the three adapters now ship
  When the docs are read (POSITIONING / why-lunaris / Zep migration + book mirrors)
  Then they describe LangGraph/CrewAI/Letta as shipped (with the Letta caveat if it degraded)
  And no "v0.4 ecosystem (shipped)" overclaim remains for anything not actually shipped
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
NEW top-level Python package `lunaris_integrations` (separate from the `lunaris` core pkg so the
unit layer imports WITHOUT loading the native cdylib; SdkLunarisClient imports `lunaris` lazily).
NO Rust change; NO change to lunaris-server / lunaris-py core. Framework deps are OPTIONAL extras.

integrations/lunaris_integrations/client.py
  @dataclass class Hit:        { id: str, content: str, score: float, source: str | None }
  class LunarisClient(Protocol):
      scope: str
      async def ingest(self, source: str, content: str, metadata: dict | None = None) -> str   # lsn
      async def recall(self, query: str, k: int = 10) -> list[Hit]
      async def forget_scope(self) -> None
  class HttpLunarisClient:      # raises ConfigError if base_url/token missing at __init__
      __init__(self, base_url: str, token: str, scope: str, *, timeout: float = 10.0)
      # POST {base_url}/v1/ingest {source,content,metadata}  (Bearer token; NO scope on the wire)
      # POST {base_url}/v1/recall {query,k}  -> [Hit]
  class SdkLunarisClient:       # lazy `import lunaris`; raises ConfigError if handle missing
      __init__(self, handle, scope: str)
  class StubLunarisClient:      # test double — records (source,content,metadata)/(query,k); canned hits
  exceptions: ConfigError, InvalidScope(value), UnsupportedFrameworkVersion(framework, found, expected)
  def namespace_to_scope(ns: tuple[str,...] | str) -> str   # "a.b" via Scope alphabet; ":"/bad -> InvalidScope

integrations/lunaris_integrations/langgraph.py        # extra: lunaris-integrations[langgraph]
  class LunarisStore(BaseStore):           # langgraph.store.base.BaseStore; version-guarded at import
      __init__(self, client_factory: Callable[[str], LunarisClient])
      async def aput(namespace: tuple[str,...], key: str, value: dict) -> None      # -> client.ingest
      async def aget(namespace, key) -> Item | None                                  # -> client.recall (key-keyed)
      async def asearch(namespace, query: str, *, limit: int = 10) -> list[SearchItem]  # -> client.recall
      # + sync put/get/search shims iff the installed BaseStore ABC declares them

integrations/lunaris_integrations/crewai.py           # extra: lunaris-integrations[crewai]
  class LunarisCrewAIStorage(Storage):     # crewai...storage interface; version-guarded
      save(self, value, metadata: dict | None = None) -> None                 # -> client.ingest
      search(self, query: str, limit: int = 3, score_threshold: float = 0.0) -> list[dict]  # -> client.recall
      reset(self) -> None                                                     # -> client.forget_scope

integrations/lunaris_integrations/letta.py            # extra: lunaris-integrations[letta]
  class LunarisArchivalConnector:          # maps Letta archival insert/search; version-guarded
      insert(self, passage) -> ...         # -> client.ingest
      search(self, query, top_k: int = 10) -> list                            # -> client.recall
      # DEGRADE PATH (per freeze flag): if the pinned Letta connector base can't be subclassed,
      # ship this as a thin shim (still client-backed, still tested) PLUS a recipe doc — NOT a full drop-in.

Tests (pytest, RED-first, StubLunarisClient — NO wheel/backend/model):
  integrations/tests/test_client.py · test_langgraph.py · test_crewai.py · test_letta.py · test_scope.py
Examples (runnable; LIVE = UAT): examples/langgraph-lunaris/ · examples/crewai-lunaris/ · examples/letta-lunaris/
Packaging: integrations/pyproject.toml — core dep httpx; extras [langgraph]/[crewai]/[letta]; lunaris optional (SdkLunarisClient).
Docs: reconcile MIGRATING-FROM-ZEP.md (+ book mirror) "v0.4 ecosystem"; lift POSITIONING/why-lunaris "roadmap"→"shipped" for the three (Letta with its caveat if degraded).
```

Status: FROZEN @ v1 — approved by Tin Dang 2026-06-15 ("Freeze as drafted (all 3 + both transports)").
Least-sure flag surfaced at freeze: [spec/contract] Letta is the riskiest of the three — its archival-memory / connector interface is heavier and less version-stable than LangGraph `BaseStore` / CrewAI `Memory`; if it can't be cleanly subclassed at the pinned version the Letta deliverable DEGRADES to a thin client-backed shim + a documented recipe (not a full drop-in class), so "all three" is partially met for Letta. Cost if wrong: Letta = recipe, not class. Secondary [spec]: sizing is L/XL for one task (3 adapters × 2 transports + examples + pytest + doc reconcile) — mitigated by the shared `LunarisClient` (each adapter ~60–100 LOC) + a stub-client unit layer (no backend); if the single verify gate proves unwieldy, split into a sub-milestone. Both accepted by Tin Dang at freeze.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: 100% of the adapter method-mappings + the client protocol + scope validation, exercised against `StubLunarisClient` (NO backend/model/wheel). The client + scope tests are framework-free (always run); per-adapter tests `pytest.importorskip("langgraph"/"crewai"/"letta")` so a missing extra SKIPs rather than fails — but the build env installs at least LangGraph so its discriminating mapping test actually runs. Live HTTP/SDK transport + a real framework end-to-end = HUMAN-UAT.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_http_client_roundtrip: HttpLunarisClient over an httpx MockTransport → ingest/recall POST {source,content,metadata}/{query,k} to /v1/ingest|/v1/recall; assert NO scope/tenant key on the wire.
  - test_transport_agnostic: a LunarisStore built over two StubLunarisClients (one tagged "http", one "sdk") records identical ingest/recall call shapes; adapter holds a LunarisClient and never branches on transport.
  - test_langgraph_store_maps: aput(ns,key,val)+asearch(ns,q) over StubLunarisClient → one ingest scoped by namespace_to_scope(ns), one recall; results mapped to LangGraph item shape. (importorskip langgraph)
  - test_crewai_storage_maps: save/search/reset → ingest / recall(k=limit) / forget_scope recorded. (importorskip crewai)
  - test_letta_connector_maps: insert/search → ingest/recall; if degraded, assert the shim still maps + the recipe doc exists. (importorskip letta)
  - test_framework_drift_rejected: a base class/shape not matching the pinned shape → UnsupportedFrameworkVersion; no partial mapping built.
  - test_missing_config_errors: HttpLunarisClient(base_url="") and SdkLunarisClient(handle=None) → ConfigError at construction; no request attempted.
  - test_invalid_scope_rejected: namespace mapping to a scope with ":" / >128 / bad char → InvalidScope before any client call (mirrors Scope::new).
  - test_docs_match_shipped: a doc-lint (grep) test — no "v0.4 ecosystem" *shipped*-overclaim remains for unshipped things; the three adapters are named as shipped (Letta with caveat if degraded). Mirrors the mem0-docs-reconcile validate_*.py approach.
</test_plan>

Tests live in: `integrations/tests/` · MUST run red (the `lunaris_integrations` package + modules don't exist yet → import-red) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `integrations/lunaris_integrations/client.py` `integrations/lunaris_integrations/langgraph.py` `integrations/lunaris_integrations/crewai.py` `integrations/lunaris_integrations/letta.py` `integrations/lunaris_integrations/__init__.py` `integrations/pyproject.toml` `integrations/README.md` `integrations/tests/` `examples/langgraph-lunaris/` `examples/crewai-lunaris/` `examples/letta-lunaris/` `docs/MIGRATING-FROM-ZEP.md` `docs/book/src/migrating/zep.md` `docs/POSITIONING.md` `docs/book/src/getting-started/why-lunaris.md`
Strategy (ordered batches): 1. `client.py` (Hit + LunarisClient Protocol + Http/Sdk/Stub impls + exceptions + namespace_to_scope) + RED `test_client.py`/`test_scope.py` → green · 2. LangGraph `LunarisStore` + test · 3. CrewAI `LunarisCrewAIStorage` + test · 4. Letta connector (or degrade to shim+recipe) + test · 5. runnable examples per framework · 6. doc reconcile (Zep "v0.4 ecosystem" + lift POSITIONING/why-lunaris to shipped) + `test_docs_match_shipped` · 7. full pytest green.
Safety rule (feature-specific): framework deps (langgraph/crewai/letta) are OPTIONAL extras — NEVER a hard dep of the `lunaris` core; `SdkLunarisClient` imports `lunaris` LAZILY (so client/scope unit tests run wheel-free); the HTTP transport puts NO scope/tenant on the wire (Bearer JWT only — honors the server's partition discipline); namespace→scope goes through `Scope.new` validation (no `:` byte-aliasing).
Code lives in: `integrations/lunaris_integrations/`
Constraints: do NOT change any test or the contract; no Rust change; no change to lunaris-server / lunaris-py core; framework + httpx are allow-listed Python deps; ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — 19 tests green across the SUPPORTED per-extra matrix: framework-free (client 5 + scope 6 + docs 4 = 15), langgraph 2, crewai 1, letta 1. (Combined-env install fails on a crewai↔letta dep conflict resolving a broken `letta` — the version-guard correctly rejects it; CI must matrix per-extra, which is the `importorskip` design.)
- [x] coverage did not decrease — net-new package; 100% of the adapter method-mappings + client protocol + scope validation exercised against `StubLunarisClient`.
- [~] no test or contract was altered during build — the FROZEN CONTRACT was untouched. ONE test fixture (`test_http_client_roundtrip`) was CORRECTED during verify: it was overfit to a fictional `{"hits":[...]}` / `{"lsn":"42"}` shape. Refute-read against the real server (`recall.rs:169` `Json(Vec<Hit>)`, `dto.rs` `IngestResponse`, `types.rs` `Hit`, `storage/types.rs` `Lsn`) showed the true shapes; the fixture now asserts the REAL contract (bare array, `text` body, byte-array `id`, `{wall_ms,counter}` lsn) — a STRENGTHENING, not a weakening.
- [x] the green was EARNED — adversarial refute-read (self, against server source) FOUND + FIXED real overfit/wiring defects the stub hid: recall `.get("hits")` on a bare array (crash), `text`-vs-`content` + byte-`id`, `metadata:null` rejected by `serde_json::Map` (now `{}`), `lsn` object→`"{wall_ms}:{counter}"`, `/v1/forget` needs `target` + soft-purge (now `BySource("")`), and the SDK `ScopedLunaris` binding has NO `forget` (now explicit `NotImplementedError`). No vacuous asserts remain.
- [x] concurrency / timing — sync adapter methods (crewai/crewai `save/search/reset`, letta `insert/search`) drive the async client via `asyncio.run`; no shared mutable state, no locks, one `httpx.AsyncClient` per client. Python — no lock-across-await concern.
- [x] no exposed secrets / injection — Bearer token sourced from env in examples; scope routed through `Scope`-alphabet validation at the boundary (`:`/bad char → `InvalidScope`, no byte-aliasing); HTTP transport puts NO scope/tenant on the wire (JWT-bound); no `eval`/shell/SQL.
- [x] layering & dependencies — `lunaris_integrations` is a pure-Python package SEPARATE from the native `lunaris` core wheel; framework deps (langgraph/crewai/letta) are OPTIONAL extras, `lunaris` SDK lazy-imported; only core dep is `httpx`. No Rust / lunaris-server / lunaris-py core change.
- [ ] a person reviewed and approved the change — **ESCALATED to human gate** (architecture/contract-reality residue below; not auto-PASSed).

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — every new symbol referenced: `client.py` exports consumed by the 3 adapters + tests + examples; `require_base_methods` guards langgraph/crewai imports + is unit-tested; `_hit_from_dict`/`_decode_id`/`_lsn_to_str` used by both HTTP+SDK recall (helpers re-verified: `_decode_id(0..16)`→16-byte hex, `_lsn_to_str({wall_ms,counter})`→`"123:4"`). Each adapter is instantiated + driven in its own per-extra test.
- [x] DEAD-CODE — no orphans, EXCEPT `HttpLunarisClient.aclose()` (public lifecycle API for closing the underlying `httpx.AsyncClient`; intentional public surface, not internally referenced — design-for-failure resource cleanup).
- [x] SEMANTIC (docs) — read in full + corrected: `MIGRATING-FROM-ZEP.md` / book `zep.md` (dropped stale "v0.4 ecosystem"); `POSITIONING.md` / `why-lunaris.md` ("roadmap"→"shipped" with the Letta shim caveat). `test_docs_match_shipped` now green and pins these. Coordinated with `mem0-docs-reconcile` (touched only the 4 named adapter-claim sites).

### GATE RECORD
Outcome: PASS — approved by Tin Dang 2026-06-15 ("PASS + commit"). The two pre-flagged residues (Letta shim+recipe; `forget_scope` SDK-binding gap → `NotImplementedError`, HTTP soft-purge) accepted as the contracted degrade; live HTTP/SDK round-trip remains HUMAN-UAT.
Residue requiring ratification (both pre-flagged at freeze):
  1. **Letta degraded to connector-shim + recipe** (freeze flag [spec/contract] materialized): Letta's archival store is server-side/DB-coupled (asyncpg) with no clean base to subclass at 0.16.8 → `LunarisArchivalConnector` ships as a tested client-backed shim + `examples/letta-lunaris/README.md` recipe. "All three" is met for Letta as a shim, not a drop-in subclass.
  2. **`forget_scope` contract-reality divergence**: the frozen `forget_scope()->None` shape held, but the transports diverge — HTTP = single-step SOFT purge (`ForgetTarget::Scope{BySource:""}`, `hard:false`); SDK = `NotImplementedError` (the `PyScopedLunaris` binding exposes only ingest/recall/dsl — no scope-forget). So CrewAI `reset()` works on HTTP, raises on the SDK transport.
  3. **Live path = UAT**: no running lunaris-server here; the HTTP/SDK transports' live round-trip + a real-framework e2e are HUMAN-UAT (per the py/ts SDK caveat). Sizing flag [spec]: delivered as ONE L/XL task (not split).
If RISK-ACCEPTED -> owner: <name> · ticket: <link> · expires: <date>   (never for a security gap)
Reviewed by: Tin Dang · date: 2026-06-15

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
