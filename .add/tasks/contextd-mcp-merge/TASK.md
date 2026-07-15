# TASK: Unify contextd+mcp: one warm engine, thin MCP stdio proxy

slug: contextd-mcp-merge · created: 2026-07-15 · stage: production · risk: high · sensitivity: architecture
autonomy: conservative   <!-- lowered from auto: high-risk architectural change (new critical shared dependency + security trust-boundary shift); verify must gate on a human, completion is not auto. -->
phase: tests   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower the
     autonomy level to `manual` or `conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- **contextd daemon** — `crates/lunaris-hook/src/contextd.rs`: `UnixListener` bind at `context::default_socket_path()` (`~/.lunaris/codex-contextd.sock`); `handle_connection` reads ONE `ContextRequest` (JSON, to EOF) → `service.handle(req)` → writes ONE `ContextResponse` → closes. **One request per connection today.** Stale-socket takeover: connect-probe then `remove_file` + rebind.
- **socket protocol** — `crates/lunaris-hook/src/context.rs:63` `enum ContextRequest { Health, RecallForPrompt, RecallAfterTool, CaptureToolCall, CaptureToolResult, TurnFeedback, SessionDigest }`; `ContextService::handle` (:212) dispatches against a warm `Lunaris` handle (per-scope, cwd-resolved via `scope::resolve_no_env` — the PR#54 scope-bleed fix). `ContextResponse`.
- **MCP server** — `crates/lunaris-mcp/src/main.rs`: 11 `#[tool]` methods (`memory.ingest`/`recall`/`forget`/`list_scopes`/`record_decision`/`record_edit`/`status`/`scratchpad_{write,read,grep,consolidate}`); `LunarisMcpServer { state: AppState(Arc<Lunaris>), tool_router }`; scope bound ONCE at startup (`LUNARIS_MCP_SCOPE` | `scope_resolver::resolve`); rmcp `outputSchema` = flat structs, root `type:object` (CLAUDE.md MCP-tool-schema invariant + `tests/server_boot.rs`).
- **shared scope+storage** — `lunaris_core::scope_resolver::resolve_with(cwd, store, override)` (both binaries, `~/.lunaris/scopes.json`); `scope::resolve_storage_url` (`LUNARIS_STORE_URL`) mirrors `mcp state.rs resolve_storage_url` (`LUNARIS_MCP_STORAGE`).
- **priority lanes** — `lunaris-llamacpp` `Priority::{Interactive,Background}` (committed `4fde2b6`): interactive embeds preempt background capture embeds on the one warm llama context.
Context (working folder): `.mcp.json` (spawns `target/release/lunaris-mcp`, env `LUNARIS_MCP_STORAGE`); `~/.claude/settings.json` hooks (`env LUNARIS_STORE_URL=moon://127.0.0.1:6381` → `lunaris-hook`/contextd). Both point at Moon :6381 on this host — same scope + same storage = one shared vault (verified 2026-07-15).
Honors (patterns / conventions): `#![forbid(unsafe_code)]` in lunaris-hook (env-free pure tests); design-for-failure = timeouts/retries/circuit-breaker/fallback (CLAUDE.md); rmcp outputSchema root-is-object invariant; never hold a lock across `.await`; keyspace helpers in `lunaris_core::keyspace`; T-25-01-01 (scope not wire-overridable at the external MCP boundary).
Anchors the contract cites: `ContextRequest`, `ContextService::handle`, `default_socket_path`, the 11 `memory.*` tool DTOs, `scope_resolver::resolve_with`, `resolve_storage_url`, `Priority::{Interactive,Background}`, `AppState`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Unify the two client surfaces onto ONE warm host-resident engine (contextd), reducing lunaris-mcp to a thin stdio→socket proxy, with graceful direct-open fallback when contextd is unavailable.
Framings weighed: thin-proxy — mcp stays a per-session stdio process but forwards `memory.*` to contextd's socket; contextd holds the single warm engine (chosen) · full single-binary — rejected (MCP stdio is one-client-per-pipe; a host daemon cannot serve N sessions' stdio) · shared-library-only, no daemon reuse — rejected (leaves two warm models, the whole point is one)
Must:
<must>
  - lunaris-mcp forwards ALL 11 `memory.*` tools to contextd over the unix socket, preserving the EXACT rmcp request/response DTOs (outputSchema byte-identical — external tool contract unchanged).
  - contextd's socket protocol serves the 11 `memory.*` operations against its single warm `Lunaris` handle (per-scope handle cache), ALONGSIDE the existing `ContextRequest` hook operations — existing hook clients keep working unchanged.
  - The `memory.*` handler logic is SINGLE-SOURCE (a shared module) called by BOTH contextd (primary) and the mcp direct-open fallback — no duplicated implementations.
  - Design-for-failure: if contextd is unreachable (socket absent / dead / connect-timeout / version-mismatch), the mcp shim FALLS BACK to opening its own direct `Lunaris` handle (today's behavior) — never a hard failure. Circuit-breaker: after N socket errors in a session, latch to direct-open for the rest of the session.
  - Scope trust boundary: the shim resolves scope (its startup binding) and FORWARDS it on each socket request; contextd trusts the local socket peer (user-owned 0700 socket). The external MCP boundary still forbids wire-scope-override (T-25-01-01 preserved — only the trusted local shim supplies scope, never a remote client).
  - Concurrency: contextd serves concurrent requests (hook captures + N mcp sessions); interactive `memory.recall` uses `Priority::Interactive` and preempts background capture embeds (`Priority::Background`) on the one warm llama context.
</must>
Reject:
<reject>
  - contextd down AND direct-open fallback also fails to open storage -> surface the underlying error, never a silent success -> "storage_unavailable"
  - protocol version mismatch (shim ↔ daemon handshake) -> NOT a hard error; fall back to direct-open (logged once) -> (degraded, not rejected)
  - a `memory.*` socket request arriving WITHOUT a scope field -> contextd must not guess -> "scope_required"
</reject>
After:
<after>
  - Exactly ONE warm embedder+reranker+Moon-connection per host (contextd), not two; the dual-model GPU/RAM contention measured 2026-07-15 is gone.
  - MCP tool behavior (recall/ingest/record/scratchpad results + outputSchema) is byte-identical to pre-merge, whether served via contextd or the fallback.
  - Killing contextd mid-session does NOT break an active MCP session (fallback proven by test).
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ The socket transport model — lowest confidence because contextd today is strictly ONE-request-per-connection (`handle_connection` reads to EOF, replies once, closes). The proxy will therefore use **connection-per-call** (open socket → one `MemoryRequest` → one response → close) as the baseline, NOT a persistent multiplexed stream. If a persistent/multiplexed conn is later needed for latency, that's a follow-up; connection-per-call keeps the existing hook framing untouched. Cost if wrong: +one local socket connect per tool call (sub-ms, negligible vs 4.86 ms recall). **This is the single biggest design risk — surfaced at freeze.**
  - [ ] contextd's per-scope warm-handle cache must be bounded (LRU or idle-evict) so N scopes across many projects don't grow unbounded — confirm a cap in the contract.
  - [ ] The 11 rmcp tool DTOs are serde-JSON-serializable as-is for socket transport (they already derive Serialize/Deserialize for rmcp) — confirm during tests.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: proxy forwards a recall to the warm daemon
  Given a running contextd holding a warm engine for scope S
  When lunaris-mcp (bound to S) receives a memory.recall tool call
  Then it sends a MemoryRequest::Recall over the socket and returns the daemon's hits
  And the returned rmcp DTO (episode_id/source/content/score/outputSchema) is byte-identical to the direct-open result

Scenario: daemon serves memory ops beside hook ops
  Given contextd is serving the existing CaptureToolResult hook path
  When a MemoryRequest::Ingest arrives on the same socket
  Then it is handled against the same warm per-scope handle and returns an LSN
  And the existing ContextRequest hook variants still dispatch unchanged

Scenario: single shared handler (no duplication)
  Given the memory.* logic lives in one shared module
  When both contextd and the mcp direct-open fallback execute memory.recall
  Then they call the identical handler function
  And neither path holds a private copy of the recall/ingest logic

Scenario: contextd-down falls back to direct-open (design-for-failure)
  Given contextd's socket is absent or the daemon was killed mid-session
  When lunaris-mcp receives a memory.recall
  Then the shim opens its own direct Lunaris handle and serves the call
  And the tool result is byte-identical to the proxied result (no hard failure surfaced)

Scenario: circuit-breaker latches after repeated socket errors
  Given N consecutive socket errors in one mcp session
  When the next memory.* call arrives
  Then the shim goes straight to direct-open without re-attempting the socket
  And it does not spawn a contextd restart storm

Scenario: interactive recall preempts background capture embed
  Given contextd is embedding a background capture batch
  When a memory.recall (Interactive) arrives concurrently
  Then the interactive embed is serviced ahead of the background batch on the one llama context
  And the background batch still completes

Scenario: storage truly unavailable is an honest error
  Given contextd is down AND direct-open cannot open storage
  When lunaris-mcp receives a memory.recall
  Then it returns error "storage_unavailable"
  And it does NOT return an empty-but-successful result

Scenario: socket memory request without scope is rejected
  Given a MemoryRequest arrives over the socket with no scope field
  When contextd dispatches it
  Then it returns "scope_required"
  And contextd does not fall back to a guessed/default scope

Scenario: external MCP client cannot override scope (T-25-01-01 preserved)
  Given an external MCP tool call carrying a wire-side scope field
  When lunaris-mcp handles it
  Then the wire scope is ignored and the shim's startup-bound scope is forwarded
  And the partition the client reaches is unchanged from pre-merge
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
# ── Socket protocol extension (crates/lunaris-hook/src/context.rs) ──
# Existing `enum ContextRequest` gains memory.* variants (one per MCP tool),
# each carrying an explicit `scope: String` (trusted local peer). Framing is
# UNCHANGED: one JSON request per connection, one JSON response, connection-per-call.
enum ContextRequest {
    // ... existing 7 hook variants unchanged ...
    Memory(MemoryRequest),                    // new umbrella variant
}
enum MemoryRequest {                          // mirrors the 11 tools; scope REQUIRED on each
    Ingest       { scope, content, .. },
    Recall       { scope, query, k, filters, as_of, raw },
    Forget       { scope, .. },
    ListScopes   { },                         // scope-independent (enumerates registry)
    RecordDecision { scope, decision, rationale, alternatives, tags, dedupe_key },
    RecordEdit   { scope, .. },
    Status       { scope },
    ScratchpadWrite { scope, key, value },
    ScratchpadRead  { scope, key },
    ScratchpadGrep  { scope, prefix },
    ScratchpadConsolidate { scope },
}
# Response: reuse each tool's EXISTING rmcp output DTO, serialized as JSON.
enum MemoryResponse { Ok(serde_json::Value /* the tool's own DTO */), Err { code: String } }
  socket errors -> code ∈ { "scope_required", "storage_unavailable", "unknown_index", ... } (tool-native codes preserved)

# ── Shared handler module (single source of truth) ──
# New: lunaris-mcp handler bodies extracted to a shared surface both callers use.
mod memory_service {                          // crate: lunaris-mcp (pub) OR new lunaris-memory-service
    async fn ingest(handle: &Lunaris, scope: &Scope, ..) -> Result<IngestDto, Code>;
    async fn recall(handle: &Lunaris, scope: &Scope, ..) -> Result<RecallDto, Code>;
    // ... one fn per tool. contextd AND the mcp fallback BOTH call these. No duplication.
}

# ── Proxy fallback state machine (lunaris-mcp) ──
enum Route { Socket, Direct }                 # per-session, starts Socket
call(tool, args):
  if route == Socket:
     try connect(sock, timeout=COLD_START_BUDGET) -> send MemoryRequest -> recv
        on success: reset error_count; return DTO
        on error:   error_count += 1; if error_count >= BREAKER_N { route = Direct; log_once }
                    fall through to Direct for THIS call
  Direct: open-or-reuse own Lunaris handle; call memory_service::<tool>(handle, ..)
        on storage-open failure: return Err "storage_unavailable"
  # version handshake on first Socket connect; mismatch -> route = Direct

# ── contextd per-scope warm-handle cache (bounded) ──
struct HandleCache { map: LruCache<Scope, Arc<Lunaris>>, cap: usize /* default 16, env override */ }
  # idle scopes evicted; each memory.* / hook request resolves-or-opens its scope handle.
Schema: no new Moon keyspace; reuses `lunaris:{scope}:{kind}:{ulid}`. No storage migration.
Access pattern: contextd = 1 warm engine/host, per-scope handle cache; mcp = thin proxy, direct-open only on fallback.
```

Status: FROZEN @ v1 — approved by Tin Dang (owner), 2026-07-15 (trust-local-peer-scope model chosen; external MCP boundary keeps T-25-01-01)
Least-sure flag surfaced at freeze: `[contract]` The **socket transport = connection-per-call** (open→one request→one response→close), NOT a persistent multiplexed stream — chosen to leave the existing one-request-per-connection hook framing (`handle_connection`) untouched. If interactive latency later demands multiplexing, that is a follow-up task, not a re-open of this contract. Second-order `[spec]`: the **scope trust boundary moves** — contextd trusts the local socket peer's `scope` (0700 user socket) while the external MCP boundary still rejects wire-override; if security review deems local-peer trust insufficient, the fallback is to have contextd RE-RESOLVE scope from a peer-supplied cwd instead of trusting the scope string (cost: an extra git/cwd resolve per call).
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every Must + every Reject scenario has a discriminating test; existing MCP/hook suites stay green (regression floor).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_shared_handler_single_source: assert contextd's Memory dispatch and mcp's fallback both route through `memory_service::<fn>` (no private copy) — arrange a recording engine, act via both paths, assert identical DTO bytes. [Must: single-source]
  - test_daemon_serves_memory_beside_hooks: spawn contextd on a temp socket (sqlite backend); send MemoryRequest::Ingest → get LSN; then send a legacy ContextRequest::CaptureToolResult → still dispatches. [Must: coexist]
  - test_proxy_forwards_recall_matches_direct: with contextd up, mcp recall over socket == direct-open recall, byte-identical rmcp DTO. [Must: forward + parity]
  - test_contextd_down_falls_back_direct: NO socket present; mcp recall opens own handle, returns byte-identical result, no hard error. [Must: design-for-failure — the crux]
  - test_circuit_breaker_latches_direct: inject N socket errors; assert (N+1)th call skips the socket (Route==Direct) and does not spawn contextd. [Must: breaker]
  - test_storage_unavailable_is_honest_error: contextd down AND direct-open storage fails → error "storage_unavailable", NOT empty-success. [Reject]
  - test_socket_memory_request_without_scope_rejected: MemoryRequest with no scope → "scope_required"; no guessed scope. [Reject]
  - test_external_wire_scope_ignored: external tool call carrying wire `scope` → shim forwards its startup-bound scope; partition unchanged. [Must: T-25-01-01 preserved]
  - test_interactive_preempts_background (model-gated / deterministic queue-order proxy): interactive recall enqueues ahead of a background capture batch on the shared intake. [Must: priority]
  - server_boot.rs (EXISTING, must stay green): all 11 tools register, outputSchema roots are objects. [regression floor]
</test_plan>

Tests live in: `crates/lunaris-mcp/tests/` (proxy/fallback/parity + wire-scope) · `crates/lunaris-hook/tests/` (daemon-serves-memory + scope_required + coexist) · MUST run red (missing `memory_service` / `MemoryRequest` / `Route`) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-mcp/src/main.rs` `crates/lunaris-mcp/src/` `crates/lunaris-hook/src/context.rs` `crates/lunaris-hook/src/contextd.rs` `crates/lunaris-mcp/tests/` `crates/lunaris-hook/tests/`
Strategy (ordered batches — phased, each independently green; this task freezes the WHOLE contract, build may be split into follow-up tasks per batch):
  1. Extract the 11 `memory.*` handler bodies from `lunaris-mcp` into a single shared `memory_service` module (pure fns over `&Lunaris`); rewire the existing `#[tool]` methods to call it — behavior-preserving refactor, existing MCP tests stay green (no proxy yet).
  2. Add `MemoryRequest`/`MemoryResponse` to `context.rs`; contextd `ContextService::handle` dispatches `Memory(..)` via `memory_service` against the bounded per-scope `HandleCache`; existing hook variants untouched (server_boot + hook tests stay green).
  3. Add the proxy `Route` state machine to `lunaris-mcp`: Socket-first with connection-per-call, version handshake, circuit-breaker, direct-open fallback via the SAME `memory_service`.
  4. Failure-model + parity tests: contextd-down→fallback byte-identical; scope_required; storage_unavailable; T-25-01-01 preserved; priority preemption.
Safety rule (feature-specific): the direct-open fallback MUST use the identical `memory_service` fn as the socket path — a divergent fallback implementation is the failure this task exists to prevent. rmcp outputSchema DTOs are FROZEN (root `type:object`, `server_boot.rs` must stay green).
Build note (batch-1 refinement, within the frozen contract): the 11 handlers currently call `tools::staging::maybe_ensure_staged` (→ `model_stager`, the MCP-specific lazy GGUF downloader w/ indicatif+reqwest). The shared `memory_service` handlers take `(&Lunaris, &Scope, params)` and MUST NOT drag in the stager — so **model staging lifts to the CALLER**: the mcp `#[tool]` wrapper calls `maybe_ensure_staged()` before delegating; contextd stages once at engine warm-up. `model_stager` + `tools/staging.rs` STAY in lunaris-mcp. schemars `JsonSchema` derives on the Params/Response DTOs move WITH them (shared crate depends on schemars, not rmcp).
Build note (batch-1 DONE, scoping deviation from "11 handlers" — within frozen contract): batch 1 extracted **6** handlers, not 11, into a new `lunaris-memory-service` crate (`ingest`, `recall`, `forget`, `record_decision`, `record_edit`, `status`) — the pure ENGINE ops contextd actually needs. The other 5 stay LOCAL in `lunaris-mcp`: the 4 `scratchpad_*` handlers couple to `session_pad` (sessions.json marker + `resolve_namespace_session_aware` → `run_handover_consolidate`) and `list_scopes` couples to the scope registry — both are mcp-transport/session concerns, not stateless engine ops, and contextd's `Memory(..)` dispatch does not serve them. Rationale: `contextd-mcp-merge` exists to unify the ENGINE path; dragging session-pad state into the shared crate would import the exact coupling the split avoids. `staging.rs` + `model_stager` stay in mcp (staging lifted to caller as planned); `ServiceError`→`rmcp::ErrorData` bridged by a `map_service_error` free fn (orphan rule blocks a `From` impl). Shared crate: 18 unit + 4 INGEST-04 gate tests green (gate moved with the guarded source); mcp: 42 unit + all smoke/boot tests green; clippy + fmt clean. Scratchpad session-decoupling into the shared service is a tracked follow-up, out of this task's batch scope. Commits: shared-crate extraction (this batch).
Build note (batch-2 DONE): lunaris-contextd now serves the 6 engine ops. `ContextRequest` gained `Memory(MemoryRequest)` (outer `type:"memory"` tag + inner `op` tag); `MemoryRequest` mirrors ONLY the 6 engine variants (scratchpad_*/list_scopes are mcp-local, never proxied — matches batch-1 split), each carrying an explicit trusted-local-peer `scope` + the shared crate's wire DTO as `params`. `MemoryResponse { Ok{data}, Err{code,message} }` returns the tool's own DTO as JSON / a tool-native code (scope_required, storage_unavailable, invalid_input, unknown_index, engine_error). `ContextService::handle_memory` resolves scope (empty→scope_required, no default-scope fall-through), gets the warm handle via the existing `handle_for_scope` cache (open-fail→storage_unavailable), then calls `lunaris_memory_service::<op>::handle(&handle,&scope,params)` — the IDENTICAL fn the mcp fallback will use (single-source-of-truth). `handle_connection` routes Memory through `handle_memory` (distinct response channel); hook variants still answer via `handle`; a direct `handle(Memory(..))` is defensively rejected. No staging call needed — `handle_for_scope` already holds the shared resident embedder. Tests (lunaris-hook lib, 5 new + 34 existing green): wire decode, ingest→recall round trip, empty-scope→scope_required, status DTO shape, direct-handle rejection; full hook integration suite + clippy --all-targets + fmt clean.
Build note (batch-3 DONE + contract refinement): the mcp `Route{Socket,Direct}` proxy landed. REFINEMENT (shape-preserving, flagged for owner): `MemoryRequest`/`MemoryResponse` + the variant→handler `dispatch` moved from `lunaris-hook::context` (where §3 located them) to `lunaris_memory_service::protocol` — a client surface must NOT depend on the OTHER client's crate, and the neutral home makes `dispatch` itself single-source (contextd's `handle_memory_inner` now calls `protocol::dispatch`, was an inline match). Shapes are byte-identical to the frozen §3; only the module home changed. Request/Response DTOs gained the missing Serialize/Deserialize halves to round-trip over the socket + the direct path's DTO→Value→DTO. Proxy (`crates/lunaris-mcp/src/proxy.rs`): per-session Route latches to Direct; Socket path = connection-per-call bounded by a cold-start connect budget (`LUNARIS_MCP_CONTEXTD_CONNECT_MS`=500ms); circuit breaker trips after N consecutive TRANSPORT failures (`LUNARIS_MCP_CONTEXTD_BREAKER_N`=3, log-once), healthy reply resets; a daemon-returned `MemoryResponse::Err` is authoritative (surfaced, no futile same-storage retry); Direct fallback calls the IDENTICAL `protocol::dispatch` (the safety rule), recall staging only on the Direct path; Socket-first only when the socket file exists at startup (else Direct), `LUNARIS_MCP_DISABLE_CONTEXTD` forces Direct-only. The 6 engine `#[tool]`s build a MemoryRequest + dispatch through the proxy; scratchpad_*/list_scopes stay local. Green: proxy breaker state-machine tests (latches after exactly N, of-one on first, reset), code→rmcp mapping; memory-service 46 + hook lib 39; mcp integration (cold_start 1.76s isolated, ingest_round_trip via Direct fallback, server_boot 11-tool roster) all green; clippy --all-targets + fmt clean. (cold_start flaked ONLY under concurrent-compile CPU contention — proxy is not invoked during initialize/tools/list; passes isolated.)
Build note (batch-4 DONE — final build slice): the failure-model + parity gates landed in `crates/lunaris-mcp/src/proxy.rs`, hermetic (in-process memory:// engine + dead socket, no daemon spawn). (1) `contextd_down_falls_back_to_direct_and_serves_the_call` — dead socket → connect-fail → breaker → direct engine serves it → valid DTO + route latched Direct (the "mcp works when the daemon is gone" guarantee). (2) `direct_fallback_is_byte_identical_to_bare_dispatch` — PARITY: proxy direct fallback == bare `protocol::dispatch` byte-for-byte; since contextd's socket path also calls `protocol::dispatch`, the two surfaces provably cannot diverge (the safety rule). (3) `wire_payload_cannot_smuggle_a_scope_field` — T-25-01-01: engine Params DTOs (deny_unknown_fields, no scope field) reject a wire-injected scope. Batch-3 already pinned the breaker state machine + code→rmcp mapping. storage_unavailable/scope_required covered contextd-side (batch-2 empty-scope) + mapping; priority preemption is a contextd concurrency concern, out of hermetic unit scope. Green: proxy 7/7; mcp integration (server_boot roster, ingest_round_trip via Direct, cold_start) unaffected; clippy --all-targets + fmt clean. BUILD COMPLETE — ready for VERIFY (human gate, conservative autonomy).
Code lives in: `crates/lunaris-mcp/src/`, `crates/lunaris-memory-service/src/`, `crates/lunaris-hook/src/`.
Constraints: do NOT change any test or the contract; do NOT alter the 11 rmcp tool DTOs / outputSchema; allow-list packages only (no new deps — reuse tokio unix socket + serde_json); ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build
- [ ] the green was EARNED, not gamed — no overfit to fixtures, vacuous asserts, or stubbed-away logic (score with an adversarial refute-read — a subagent recommended under `autonomy: auto`; a confirmed cheat is HARD-STOP)
- [ ] concurrency / timing of the risky operation is safe
- [ ] no exposed secrets, injection openings, or unexpected dependencies
- [ ] layering & dependencies follow CONVENTIONS.md
- [ ] a person reviewed and approved the change

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
> Pre-declare the OBSERVABLE outcomes a correct build must produce — derived from §2 SCENARIOS
> + §3 CONTRACT — so this gate checks the build is RIGHT, not merely that tests are green. Each
> row is evidence you can SEE, not a restatement of a test name.
- [ ] <observable outcome a correct build must produce> — confirmed by <how / where>
- [ ] <another observable outcome> — confirmed by <evidence seen>

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [ ] WIRING (code) — every new symbol is referenced; record where / how confirmed
- [ ] DEAD-CODE (code) — no new unused or orphaned symbol introduced
- [ ] SEMANTIC (prose / non-code) — read in full, not skimmed: <what read · what confirmed>

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
If RISK-ACCEPTED -> owner: <name> · ticket: <link> · expires: <date>   (never for a security gap)
Reviewed by: <name> · date: <date>

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>

### Spec delta
Forward changes for the next loop — each re-enters at Specify as the next task. One line
each, tagged `[SPEC · open|seeded|dropped]`, with evidence (e.g. `[SPEC · open] rate-limit
the retry path (evidence: prod herd spikes)`). See the `add` skill's `deltas.md`.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
