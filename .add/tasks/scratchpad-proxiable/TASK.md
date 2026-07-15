# TASK: Make scratchpad ops proxiable: extract to shared crate + route through contextd

slug: scratchpad-proxiable · created: 2026-07-15 · stage: production
autonomy: conservative   <!-- lowered from auto: this is a wire-protocol + trust-boundary change; verify gates on the human -->
phase: tests   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
sensitivity: architecture,security   <!-- new socket wire variants + scope/namespace trust boundary -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-mcp/src/tools/scratchpad_write.rs:handle(state:&AppState, ScratchpadWriteParams) -> Result<ScratchpadWriteResponse, ToolError>` — resolves namespace session-aware, `WorkingMemory::new(state.lunaris, state.scope, ns).write(key, value)`. Params: `{key, value, namespace:Option}`. Resp `{lsn}`.
- `crates/lunaris-mcp/src/tools/scratchpad_read.rs:handle(...)` — `maybe_ensure_staged()` + resolve ns + `wm.read(key)`. Params `{key, namespace:Option}`. Resp `{found, value:Option}`.
- `crates/lunaris-mcp/src/tools/scratchpad_grep.rs:handle(...)` — `maybe_ensure_staged()` + resolve ns + `wm.grep(pattern)`. Params `{pattern, namespace:Option}`. Resp `{entries:[{source,value}]}`.
- `crates/lunaris-mcp/src/tools/scratchpad_consolidate.rs` — `handle_inner(state, params, timeout)` (3 guards: `queue_native` / `consolidator_pipeline().is_enabled()` / hard timeout; flat `{status,promotions,archives,message?}` resp) AND `run_handover_consolidate(state)` (whole-scope `consolidate_unfiltered()`, warn-and-continue, no error surfaced).
- `crates/lunaris-mcp/src/tools/staging.rs:{validate_namespace, resolve_namespace, resolve_namespace_session_aware(state, ns), maybe_ensure_staged}` — namespace validator (`[A-Za-z0-9_\-./]{1..=128}`, no `:`) + session resolver that reads the sessions.json marker and calls `run_handover_consolidate` on a session change.
- `crates/lunaris-mcp/src/session_pad.rs:{sessions_file_path, active_session_at, default_namespace, take_pending_handover_at}` — mcp-LOCAL sessions.json marker maintained by lunaris-hook. STAYS in mcp (client-machine state; contextd has no marker).
- `crates/lunaris-memory-service/src/protocol.rs:{MemoryRequest (6 variants), MemoryResponse, dispatch, MemoryRequest::needs_embedder}` — the wire contract + single dispatch both peers call.
- `crates/lunaris-mcp/src/proxy.rs:MemoryProxy::{dispatch, try_socket, direct, note_transport_strike}` — Socket→Direct circuit breaker; `direct()` stages the embedder iff `req.needs_embedder()`.
- `crates/lunaris-mcp/src/main.rs` — the 4 scratchpad `#[tool]` methods (currently call `tools::scratchpad_*::handle(&self.state, params)` directly, NOT proxied).
- `crates/lunaris-hook/src/context.rs:handle_memory_inner` + `contextd.rs` — already route ANY `ContextRequest::Memory(req)` through `protocol::dispatch`; new variants are served for free once dispatch handles them.

Context (working folder): the contextd-mcp-merge (PR #56, MERGED) left the 5 session/registry tools local-only. This task moves the 4 scratchpad tools onto the shared dispatch so a socket-mode mcp needs no second engine for them. `list_scopes` (registry) stays local — out of scope.
Honors (patterns / conventions): batch-1 staging-lift (a CALLER concern — `needs_embedder`); shared handler signature `handle(lunaris:&Lunaris, scope:&Scope, params) -> Result<_, ServiceError>`; DTO discipline (`deny_unknown_fields`, no wire `scope` field on Params); MCP outputSchema root MUST be `type:object` (flat structs, no tagged enum); INGEST-04 one `atomic_write`; never lock across `.await`; `#![forbid(unsafe_code)]` in lunaris-hook.
Anchors the contract cites: `MemoryRequest`, `dispatch`, `needs_embedder`, `MemoryProxy::dispatch`, the 4 scratchpad `handle` fns, `handover::handle`, `resolve_namespace_session_aware`, `run_handover_consolidate`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: proxiable scratchpad — the 4 scratchpad engine ops execute through the ONE shared `dispatch`, reachable over the contextd socket, with session-marker resolution kept mcp-side.

Framings weighed:
- **Pre-resolve namespace caller-side, carry it on `params.namespace` (chosen)** — mcp resolves the session-aware namespace locally (reads its own sessions.json), stuffs the resolved `Some(ns)` into `params.namespace`, and dispatches. The shared handler signature stays identical to the 6 engine handlers (`handle(lunaris, scope, params)`); no new wire field. Handover-consolidate becomes its own dispatch variant so it runs on contextd's warm engine (which owns the pad), not a second local engine.
- Lift namespace to a separate handler arg + wire field (`{scope, namespace, params}`) — rejected: diverges the signature from the 6 engine handlers and duplicates a value already on `params`.
- Move session-marker reading into the shared crate / contextd — rejected: sessions.json is per-client-machine state that lunaris-hook maintains next to the mcp process; contextd (possibly a different process/host later) has no marker. Session detection is inherently a caller concern.

Must:
<must>
  - Move `scratchpad_{write,read,grep,consolidate}` handlers + a new `handover` handler into `lunaris-memory-service`, signature `handle(lunaris:&Lunaris, scope:&Scope, params) -> Result<Resp, ServiceError>` (consolidate keeps its injectable-timeout `handle_inner`; handover is `handle(lunaris, scope) -> HandoverResponse`, warn-and-continue → always `Ok`).
  - Move `validate_namespace` + the "resolve `params.namespace` → String (default `scratchpad/`)" logic into the shared crate; each shared handler resolves+validates its own namespace from `params.namespace`.
  - Add 5 variants to `MemoryRequest`: `ScratchpadWrite/Read/Grep/Consolidate {scope, params}` + `ScratchpadHandover {scope}`; `dispatch` maps each to its handler.
  - `needs_embedder()` returns true for `Recall`, `ScratchpadRead`, `ScratchpadGrep` (all touch vector search); false for Write/Consolidate/Handover.
  - The 4 mcp scratchpad `#[tool]` methods route through `self.proxy.dispatch(&self.state, req)` (Socket→Direct breaker), decoding the JSON `data` back into the tool's `Json<Resp>`.
  - Session resolution stays mcp-side: `resolve_namespace_session_aware` reads the marker; on a detected session change it triggers the handover THROUGH the proxy (`MemoryRequest::ScratchpadHandover{scope}`), so the drain runs on the warm engine holding the pad. Warn-and-continue: a handover dispatch failure NEVER errors the triggering scratchpad op.
  - Socket path and Direct path execute byte-identical engine logic (single `dispatch`) — proven by a parity test.
  - contextd serves all 5 new variants (via the existing `handle_memory` → `dispatch`).
</must>
Reject:
<reject>
  - namespace containing `:` or outside `[A-Za-z0-9_\-./]{1..=128}` -> `invalid_input`
  - empty scope string on any variant -> `scope_required`
  - a wire `params` carrying an unknown field (e.g. smuggled `scope`/`tenant`) -> serde `deny_unknown_fields` reject -> `invalid_input`
  - consolidate on a non-native-queue backend -> `status:"unsupported_backend"` (Ok body, not an error) ; worker live -> `status:"worker_conflict"` ; drain over cap -> `status:"timeout"`
</reject>
After:
<after>
  - An mcp `#[tool]` scratchpad call with a live contextd socket runs the op on contextd's warm engine (no second engine opened for that op); with the socket absent/broken it transparently falls back to Direct and still succeeds.
  - `grep -rn 'MemoryRequest::Scratchpad' crates/lunaris-mcp/src` shows the tool methods build requests; the scratchpad `handle` bodies exist ONLY in `lunaris-memory-service`.
  - A session change still consolidates the previous pad exactly once, now on the warm engine.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ **Handover must proxy, and clearing the marker AFTER a best-effort dispatch keeps one-drain-per-switch** — lowest confidence because it splits detection (mcp marker) from execution (contextd drain) across a socket; if wrong (e.g. a socket failure skips the drain but the flag is already cleared), the previous session's pad is never consolidated. Cost: orphaned pending events (not data loss — they stay in the pad, just un-promoted). Mitigation: `take_pending_handover_at` already clears only on read — the handover is best-effort warn-and-continue exactly like today's in-process path; the ONLY change is the drain now runs on the warm engine over the socket instead of a local engine. Same at-most-once semantics, same failure posture.
  - [ ] Read/grep truly need the embedder on the Direct path (fused Vector+Keyword plan) — confirm `needs_embedder` must include them, else Direct-fallback read/grep fail to stage. (Confirmed by the existing `maybe_ensure_staged()` at the top of both handlers.)
  - [ ] Moving `validate_namespace` to shared does not orphan the mcp-side `resolve_namespace_session_aware`, which still validates an explicit wire namespace early — it re-exports/reuses the shared validator.
  - [ ] contextd needs no new code — adding variants to `dispatch` is sufficient because `handle_memory` already forwards any `MemoryRequest`.
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: scratchpad_write over the socket runs on the warm engine
  Given a contextd socket is present and a scope "proj-a"
  When mcp handles memory.scratchpad_write{key:"k", value:1}
  Then contextd's dispatch runs scratchpad_write::handle and returns {lsn}
  And mcp opened no second engine for the write

Scenario: contextd down -> Direct fallback still writes
  Given no contextd socket (or a broken one)
  When mcp handles memory.scratchpad_write{key:"k", value:1}
  Then the proxy trips to Direct, runs the same dispatch locally, returns {lsn}
  And the tool call succeeds (no error surfaced)

Scenario: byte-identical parity socket vs direct
  Given the same engine state
  When the same ScratchpadRead request runs via Direct dispatch and via the socket dispatch
  Then both return byte-identical JSON `data`

Scenario: read/grep stage the embedder on the Direct path
  Given no socket, so the proxy uses Direct
  When mcp handles memory.scratchpad_read / _grep
  Then needs_embedder() is true and the embedder is staged before the vector plan runs

Scenario: session change consolidates the previous pad once, on the warm engine
  Given the sessions.json marker flags a pending handover for scope "proj-a"
  When mcp resolves the namespace for the next scratchpad op
  Then it dispatches ScratchpadHandover{scope:"proj-a"} through the proxy (contextd drains consolidate_unfiltered)
  And the triggering scratchpad op still succeeds even if the handover dispatch fails
  And the marker's pending flag is cleared

Scenario: wire namespace with ':' is rejected
  Given any scratchpad variant
  When params carry namespace "a:b"
  Then dispatch returns invalid_input
  And no WorkingMemory write/read occurs

Scenario: smuggled scope field is rejected at the DTO boundary
  Given a wire params object {key:"k", value:1, scope:"other"}
  When it is deserialized into ScratchpadWriteParams
  Then serde deny_unknown_fields rejects it -> invalid_input
  And the server-bound scope is never overridden

Scenario: consolidate on sqlite returns unsupported_backend (not an error)
  Given a memory:// backend (queue_native=false)
  When mcp handles memory.scratchpad_consolidate
  Then dispatch returns Ok {status:"unsupported_backend", promotions:0, archives:0, message:..}
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
# Wire protocol — lunaris_memory_service::protocol (extends the FROZEN-@v1 6-variant enum)

MemoryRequest (add 5 variants; #[serde(tag="op", rename_all="snake_case")]):
  ScratchpadWrite       { scope: String, params: ScratchpadWriteParams }
  ScratchpadRead        { scope: String, params: ScratchpadReadParams }
  ScratchpadGrep        { scope: String, params: ScratchpadGrepParams }
  ScratchpadConsolidate { scope: String, params: ScratchpadConsolidateParams }
  ScratchpadHandover    { scope: String }
  # namespace is NOT a top-level wire field: it rides params.namespace (Option),
  # pre-resolved to Some(session-aware ns) by the mcp caller before dispatch.
  # Handover carries no params (whole-scope unfiltered drain).

MemoryRequest::scope()          -> &str        (extended match: all 11 variants)
MemoryRequest::op()             -> &'static str (adds the 5 labels)
MemoryRequest::needs_embedder() -> bool         (Recall | ScratchpadRead | ScratchpadGrep)

dispatch(lunaris, scope, req) -> Result<Value, ServiceError>   (adds 5 arms)
  ScratchpadWrite       -> scratchpad_write::handle(lunaris, scope, params)        -> {lsn:String}
  ScratchpadRead        -> scratchpad_read::handle(lunaris, scope, params)         -> {found:bool, value:Option<Value>}
  ScratchpadGrep        -> scratchpad_grep::handle(lunaris, scope, params)         -> {entries:[{source:String, value:Value}]}
  ScratchpadConsolidate -> scratchpad_consolidate::handle(lunaris, scope, params)  -> {status:String, promotions:usize, archives:usize, message?:String}
  ScratchpadHandover    -> handover::handle(lunaris, scope)                        -> {status:String}   # ok|skipped_no_queue|skipped_worker_conflict|timeout ; always Ok (warn-and-continue)

# Shared-crate handlers (moved from lunaris-mcp/src/tools/*):
lunaris_memory_service::scratchpad_{write,read,grep}::handle(lunaris:&Lunaris, scope:&Scope, params) -> Result<Resp, ServiceError>
lunaris_memory_service::scratchpad_consolidate::{handle(lunaris,scope,params), handle_inner(lunaris,scope,params,timeout:Duration)}
lunaris_memory_service::handover::handle(lunaris:&Lunaris, scope:&Scope) -> HandoverResponse   # infallible (Ok), 3 guards, whole-scope consolidate_unfiltered
lunaris_memory_service::namespace::{validate(&str)->Result<(),ServiceError>, resolve(Option<String>)->Result<String,ServiceError>}  # default "scratchpad/"

# Params/Response DTOs move to the shared crate (Serialize+Deserialize+JsonSchema, deny_unknown_fields on Params);
# lunaris-mcp #[tool] methods use them as the input/Json<T> output type (re-export path).

# mcp caller (lunaris-mcp/src/main.rs #[tool] + staging.rs):
resolve_namespace_session_aware(proxy:&MemoryProxy, state:&AppState, ns:Option<String>) -> Result<String, ToolError>
  reads sessions.json marker; on pending handover -> proxy.dispatch(state, ScratchpadHandover{scope}) (result logged, NEVER propagated); returns default_namespace(active)
each scratchpad #[tool]: ns = resolve_namespace_session_aware(&self.proxy, &self.state, params.namespace)?; params.namespace = Some(ns);
  data = self.proxy.dispatch(&self.state, MemoryRequest::Scratchpad*{scope, params}).await?; decode_dto(data)

Errors (unchanged code set): scope_required | invalid_input | storage_unavailable | unknown_index | engine_error
Trust boundary (UNCHANGED from v1): contextd trusts the local 0700-socket peer's `scope` string; the external MCP boundary forbids wire scope-override (Params carry no scope field + deny_unknown_fields).
```

Status: FROZEN @ v1 — approved by Tin Dang (2026-07-15)

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: parity with the moved handlers' existing tests (no regression) + new wire/routing tests.
Plan (one test per scenario):
<test_plan>
  - shared: scratchpad_{write,read,grep,consolidate} handler tests MOVED to `crates/lunaris-memory-service/src/scratchpad_*.rs` — construct (lunaris, scope) directly, no AppState.
  - shared: `namespace::validate` colon/charset/length cases (moved from staging.rs).
  - protocol: ScratchpadRead round-trips through dispatch; needs_embedder true for Read/Grep, false for Write/Consolidate/Handover.
  - protocol: handover::handle on memory:// returns {status:"skipped_no_queue"} (guard 1), Ok.
  - proxy: scratchpad_write parity — Direct dispatch data == bare dispatch data (byte-identical).
  - proxy: contextd-down -> Direct fallback returns {lsn}.
  - proxy: wire params with smuggled `scope` field -> invalid_params (deny_unknown_fields).
  - mcp: resolve_namespace_session_aware on a pending-handover marker dispatches ScratchpadHandover and clears the flag; a failing handover dispatch does NOT error the caller.
  - contextd: a ScratchpadWrite over the socket returns {lsn} from the warm engine (integration).
</test_plan>

Tests live in: `crates/lunaris-memory-service/src/`, `crates/lunaris-mcp/src/proxy.rs`, `crates/lunaris-mcp/src/tools/staging.rs`, `crates/lunaris-hook/tests/`. MUST run red before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch):
`crates/lunaris-memory-service/src/lib.rs` `crates/lunaris-memory-service/src/protocol.rs` `crates/lunaris-memory-service/src/scratchpad_write.rs` `crates/lunaris-memory-service/src/scratchpad_read.rs` `crates/lunaris-memory-service/src/scratchpad_grep.rs` `crates/lunaris-memory-service/src/scratchpad_consolidate.rs` `crates/lunaris-memory-service/src/handover.rs` `crates/lunaris-memory-service/src/namespace.rs` `crates/lunaris-mcp/src/main.rs` `crates/lunaris-mcp/src/proxy.rs` `crates/lunaris-mcp/src/tools/mod.rs` `crates/lunaris-mcp/src/tools/staging.rs` `crates/lunaris-mcp/src/tools/scratchpad_write.rs` `crates/lunaris-mcp/src/tools/scratchpad_read.rs` `crates/lunaris-mcp/src/tools/scratchpad_grep.rs` `crates/lunaris-mcp/src/tools/scratchpad_consolidate.rs`

Strategy (ordered batches):
1. Extract handlers + DTOs + namespace helper into `lunaris-memory-service` (move tests, rewrite fixtures to (lunaris,scope)).
2. Add 5 `MemoryRequest` variants + dispatch arms + `handover` handler + `needs_embedder`; protocol tests green.
3. Route the 4 mcp scratchpad `#[tool]` methods through the proxy; retarget `resolve_namespace_session_aware` handover to `proxy.dispatch(ScratchpadHandover)`; mcp shims re-export the shared DTOs; proxy parity/fallback/smuggle tests green.
4. contextd integration test (ScratchpadWrite over the socket) + full affected-suite green.

Safety rule (feature-specific): handover dispatch is warn-and-continue — a failed/absent handover MUST NOT error the triggering scratchpad op; the marker flag clears only via `take_pending_handover_at` AFTER the best-effort dispatch. No lock held across `.await`.
Constraints: do NOT change any test or the contract; namespace `:`-rejection preserved; MCP outputSchema roots stay `type:object`.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build
- [ ] the green was EARNED (adversarial refute-read of the parity + fallback tests)
- [ ] handover warn-and-continue: a failed handover dispatch does not error the scratchpad op (timing/failure test)
- [ ] no wire scope-override path (deny_unknown_fields on all 4 scratchpad Params)
- [ ] layering: no rmcp in lunaris-memory-service; staging stays a caller concern
- [ ] a person reviewed and approved the change

### Build expectations
- [ ] socket-mode scratchpad_write opens NO second engine — confirmed by contextd integration test + no embedder-stage log on the write path
- [ ] scratchpad handle bodies exist ONLY in lunaris-memory-service — confirmed by `grep -rn 'WorkingMemory::new' crates/lunaris-mcp/src/tools` returning nothing for the moved handlers

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
Reviewed by: <name> · date: <date>

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch: handover skip rate (socket failures), Direct-fallback rate on scratchpad ops.

### Spec delta
- [SPEC · open] once scratchpad proxies, a pure-socket mcp needs NO local engine — the deferred lazy-embedder / skip-engine-open task can finally land (evidence: this task removes the last engine-coupled tool group).

### Competency deltas
- (fill at observe)
