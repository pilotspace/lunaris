# TASK: Make scratchpad ops proxiable: extract to shared crate + route through contextd

slug: scratchpad-proxiable · created: 2026-07-15 · stage: production
autonomy: conservative   <!-- lowered from auto: this is a wire-protocol + trust-boundary change; verify gates on the human -->
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
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

Least-sure flag surfaced at freeze: [contract] handover-across-socket at-most-once — splitting
detection (mcp marker) from the drain (contextd) could orphan a pad if the socket fails after the
marker flag is already cleared. Mitigation held: same warn-and-continue posture as today's in-process
path (`take_pending_handover_at` still clears only on read); the only change is the drain runs on the
warm engine over the socket. Same at-most-once semantics; orphaned events stay in the pad (un-promoted),
retried at the next switch — not data loss. Second [spec] flag: `needs_embedder` MUST include
ScratchpadRead/Grep (fused Vector+Keyword plan) or Direct-fallback read/grep fail to stage — confirmed
by the existing `maybe_ensure_staged()` at the top of both handlers, now encoded in `needs_embedder`.

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

- [x] all tests pass — shared 49 (44 batch-1 + 5 protocol); mcp default staging 2 / proxy 7 / session_pad 5; contextd handle_memory 5; embedded-moon on a REAL Moon: round-trip, session-handover-rotates-and-drains (crux), consolidate guard2/guard3/wired, bootstrap-launches-moon — all green.
- [x] coverage did not decrease — every moved handler test relocated (write 4, read 3, grep 3, namespace 12, consolidate 2 non-Moon + 3 Moon); +5 protocol, +3 proxy scratchpad, +2 staging, +2 contextd.
- [x] no test or contract was altered during build — frozen §3 unchanged; only NEW tests added + moved verbatim.
- [x] the green was EARNED — parity test asserts byte-identical Direct==bare dispatch; handover crux test is discriminating (drain finds 0 ⟺ handover ran; "without the handover this drains the two enqueued events"); consolidate wired test seeds under the REAL scope (scope-dev discriminator).
- [x] handover warn-and-continue — handover::handle is infallible (always Ok, advisory status); `handle_memory_scratchpad_handover_is_ok_and_skips` proves Ok on no-queue; `resolve_namespace_session_aware` logs+swallows a proxy.dispatch error, never propagates.
- [x] no wire scope-override path — deny_unknown_fields on all 4 scratchpad Params; `wire_payload_cannot_smuggle_a_scope_field` (proxy) + `params_reject_smuggled_scope` (shared) green.
- [x] layering — lunaris-memory-service has NO rmcp dep (ServiceError→rmcp maps at the mcp boundary); staging (model-stage + session marker) stays mcp-side; clippy `-D warnings --all-targets` clean on all 3 crates.
- [ ] a person reviewed and approved the change — THIS GATE.

### Build expectations
- [x] socket-mode scratchpad_write opens NO second engine — `handle_memory_scratchpad_write_then_read_round_trip` runs the op on contextd's warm per-scope handle; write's `needs_embedder=false` so the Direct path never stages either.
- [x] scratchpad handle bodies exist ONLY in lunaris-memory-service — `crates/lunaris-mcp/src/tools/scratchpad_{write,read,grep}.rs` deleted; `grep WorkingMemory::new crates/lunaris-mcp/src` returns nothing.

### GATE RECORD
Outcome: PASS
Reviewed by: Tin Dang · date: 2026-07-15
Evidence: shared 49 + mcp-default 14 + contextd 5 + embedded-moon (real Moon) all green; parity
byte-identical; clippy -D warnings --all-targets clean on all 3 crates; frozen §3 unaltered.
Residual [contract] flag (handover at-most-once) mitigated to the prior in-process posture and
exercised green by the session-handover-rotates-and-drains Moon test. Security: deny_unknown_fields
+ two smuggle-reject tests; trust boundary unchanged from v1.

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch: handover skip rate (socket failures), Direct-fallback rate on scratchpad ops.

### Spec delta
- [SPEC · open] once scratchpad proxies, a pure-socket mcp needs NO local engine — the deferred lazy-embedder / skip-engine-open task can finally land (evidence: this task removes the last engine-coupled tool group).

### Competency deltas
- (fill at observe)
