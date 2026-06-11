# TASK: SessionEnd envelope + durable last-active-session marker + switch detection in lunaris-hook

slug: session-switch-detect · created: 2026-06-11 · stage: production
phase: done   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: session-switch-detect — make a coding-agent session switch OBSERVABLE to Lunaris. lunaris-hook learns SessionEnd, keeps a durable last-active-session marker, and emits a typed SwitchObserved outcome that scratchpad-handover and session-context-inject consume.

Ground facts (2026-06-11):
  - lunaris-hook is a per-event short-lived process (one stdin envelope -> one ingest -> exit). envelope.rs parses 4 events (PreToolUse/PostToolUse/Stop/SessionStart); SessionEnd is parsed as Unknown and dropped (exit 0 no-op). All payloads carry session_id.
  - There is NO session state anywhere: dedupe is content-hash only; the only durable hook/MCP shared state is ~/.lunaris/scopes.json (ScopeStore pattern: load/save whole-file).
  - The MCP server NEVER receives the agent's session_id (stdio tools have no session context) — so any "active session" knowledge MUST live where both binaries can read it: a sibling file in ~/.lunaris/ is the only existing pattern.
  - Claude Code emits SessionEnd with a `reason` (clear/logout/exit/other) but crashes/kills DO NOT emit it — switch detection must work from SessionStart alone (marker mismatch), with SessionEnd as a hint.
  - HOOK-06 emergency-drop contract: the hook must NEVER block or fail the agent on storage trouble; marker IO gets the same discipline.

Framings weighed: durable marker file in ~/.lunaris/sessions.json maintained by the hook, read by MCP (chosen — only pattern that bridges the MCP session-id blindness; same trust domain as scopes.json) · store the marker IN Lunaris as a scratchpad meta key (rejected — MCP could read it, but the hook would need a storage round-trip + recall on EVERY event, and a stalled store would break detection; local file honors HOOK-06) · rely on SessionEnd alone (rejected — crashes never emit it).

Scope boundary: crates/lunaris-hook/ only (envelope.rs + new session_marker.rs + ingest/main wiring). NO MCP-side changes (task scratchpad-handover owns the MCP read side). NO consolidation, NO namespace rotation, NO context injection — this task only DETECTS and RECORDS.

Must:
<must>
  - envelope.rs parses SessionEnd (hook_event_name, session_id, cwd, transcript_path?, reason?) into a typed HookEvent::SessionEnd; unknown events still no-op exit 0
  - new session_marker module: per-scope marker in ~/.lunaris/sessions.json — {scope: {active_session_id, ended (bool), updated_at}}; atomic write (tmp + rename) mirroring the scopes.json store pattern; corrupt/missing file treated as no-marker (warn, never fail)
  - SessionStart handler: read marker; if a different session_id is active -> SwitchObserved {previous_session_id, new_session_id, previous_ended} emitted as (a) a single-line JSON stderr log and (b) metadata fields (switch_from, switch_prev_ended) on the session_start episode it already ingests; then write the new marker
  - SessionEnd handler: ingest a session_end episode (source claude-code:session_end, reason in metadata) and set ended=true on the marker (session_id retained for handover)
  - failure design: every marker read/write error degrades to warn + continue (exit codes unchanged); marker IO happens BEFORE the ingest timeout budget so HOOK-06 semantics are untouched
  - session_id is sanitized to the Scope alphabet [A-Za-z0-9_\-.] before any use in keys/namespaces (defense for task 2)
</must>
Reject:
<reject>
  - SessionStart with marker IO failure -> still ingests + exits 0 (never blocks the agent)
  - sessions.json containing invalid JSON -> treated as absent, overwritten on next write, warn logged (never a parse-crash)
  - a SessionEnd for a session_id that does not match the marker -> episode still ingested; marker untouched; warn (stale/out-of-order event)
</reject>
After:
<after>
  - killing session A (no SessionEnd) and starting session B yields SwitchObserved{A,B,previous_ended:false} — crash-safe detection
  - a clean end->start cycle yields SwitchObserved{A,B,previous_ended:true}
  - ~/.lunaris/sessions.json always names the active session per scope — the read-side contract task 2 (MCP) builds on
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Claude Code SessionEnd payload shape (exact optional fields) — lowest confidence because we type it from docs, not a captured envelope; if wrong: parse falls to Unknown (exit 0 no-op) and detection still works via SessionStart-marker mismatch — degraded, not broken. Mitigation: serde defaults on every field except hook_event_name+session_id.
  - [x] Concurrent hook invocations racing the marker file — bounded: events for one scope are serialized by the agent in practice; atomic rename keeps the file always-valid; last-writer-wins is acceptable for a hint marker.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: crash switch detected from SessionStart alone
  Given sessions.json marks session A active (ended=false) for scope S
  When a SessionStart envelope for session B in scope S is processed
  Then stderr carries one JSON line {"event":"session_switch","from":"A","to":"B","prev_ended":false}
  And the ingested session_start episode metadata contains switch_from=A, switch_prev_ended=false
  And sessions.json now marks B active
  And exit code is 0

Scenario: clean end -> start cycle
  Given session A active
  When SessionEnd(A, reason=logout) then SessionStart(B) are processed
  Then the session_end episode is ingested with reason=logout and the marker shows ended=true
  And the subsequent switch reports prev_ended=true

Scenario: first session ever (no marker)
  Given no sessions.json (or no entry for scope S)
  When SessionStart(A) is processed
  Then no switch is reported, A becomes the active marker, episode ingested as today

Scenario: marker store corrupt or unwritable
  Given sessions.json contains invalid JSON (or the dir is read-only)
  When any session event is processed
  Then a warn line is logged, the event is still ingested, exit code unchanged
  And a corrupt file is replaced by a valid one on the next successful write

Scenario: stale SessionEnd ignored for the marker
  Given session B is active
  When SessionEnd(A) arrives (out of order)
  Then the session_end episode is still ingested but the marker still names B
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
DELIVERABLES (hook-side only; the MCP read side belongs to scratchpad-handover):
  crates/lunaris-hook/src/envelope.rs     — HookEvent::SessionEnd(SessionEndPayload)
    payload: { hook_event_name: String, session_id: String, cwd: Option<String>,
               transcript_path: Option<String>, reason: Option<String>, timestamp: Option<String> }
    (serde defaults on all optionals; unknown event names still -> Unknown)
  crates/lunaris-hook/src/session_marker.rs — NEW
    file: ~/.lunaris/sessions.json   shape: { "<scope>": { "active_session_id": "<sanitized>",
            "ended": bool, "updated_at": "<rfc3339>" } }
    API: read_marker(scope) -> Option<Marker> (None on any error, warn)
         write_marker(scope, session_id, ended) -> () (atomic tmp+rename; warn on error)
         observe_start(scope, new_session_id) -> Option<SwitchObserved{previous, prev_ended}>
    override env: LUNARIS_SESSIONS_FILE (tests + non-default homes)
  crates/lunaris-hook/src/ingest.rs / lib.rs — SessionStart gains switch metadata
    (switch_from, switch_prev_ended) + stderr JSON line {"event":"session_switch",...};
    SessionEnd ingests source "claude-code:session_end" with reason metadata
Error semantics: marker IO NEVER changes exit codes (0/64/65/66/73 as today);
  session_id sanitized to [A-Za-z0-9_\-.] (replacement char "-") before keying.
Evidence protocol:
  red   = unit tests written first: envelope SessionEnd parse; marker round-trip;
          crash-switch detection; corrupt-file tolerance; stale-SessionEnd — all fail
          on current main (SessionEnd -> Unknown, no marker module)
  green = cargo test -p lunaris-hook green; plus one end-to-end stdin-driven test
          (spawn the binary twice with SessionStart A then B against a temp
          LUNARIS_SESSIONS_FILE + sqlite store, assert the stderr switch line)
Schema: no MCP, no engine, no SDK changes; sessions.json is additive.
```

Status: FROZEN 2026-06-11 — approved by Tin Dang ("Freeze it") at the bundle decision point
Least-sure flag surfaced at freeze:
  ⚠ [spec] SessionEnd payload field shape is typed from Claude Code docs, not a captured live envelope — serde-defaulted optionals mean a mismatch degrades to Unknown/no-op rather than a crash, and crash-path detection (marker mismatch at SessionStart) carries the feature regardless. Residual cost if wrong: SessionEnd episodes missing until one captured envelope corrects the struct.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every scenario above = one test; cargo test -p lunaris-hook stays the gate.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - envelope::session_end_parses / unknown_still_noop (red: SessionEnd currently -> Unknown)
  - session_marker::round_trip / first_session_no_switch / crash_switch_detected /
    clean_cycle_prev_ended_true / corrupt_file_treated_absent / stale_end_keeps_marker
    (red: module does not exist)
  - e2e: spawn lunaris-hook twice (SessionStart A, SessionStart B) with
    LUNARIS_SESSIONS_FILE + sqlite temp store; assert stderr switch JSON + exit 0
  - sanitizer: session_id "a:b/c" keys as "a-b-c" (alphabet defense)
</test_plan>

Tests live in: `crates/lunaris-hook/src/` unit mods + `crates/lunaris-hook/tests/` · red run recorded before build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): <e.g. debit+credit in one atomic transaction>
Code lives in: `./src/`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — cargo test -p lunaris-hook: 67 passed / 20 suites (was 2+9 red); clippy --all-targets clean
- [x] coverage did not decrease — +12 tests (envelope_session_end x3, session_switch x9 incl. the binary-spawn e2e); zero existing tests touched
- [x] no test or contract was altered during build — red suite committed first (honest red/green pair: red commit reproduces the measured 2+9 failures with stubs)
- [x] concurrency / timing — marker writes are atomic tmp+rename (file always valid); last-writer-wins acceptable for a hint marker (bundle assumption [x]); no locks, no await-holding
- [x] no exposed secrets / injection / new deps — marker stores only sanitized session ids + timestamps; zero new dependencies (dirs/chrono/serde already in the crate)
- [x] layering — hook-side only (frozen boundary held): no MCP, engine, or SDK changes; exit codes 0/64/65/66/73 untouched (e2e asserts exit 0 across marker activity)
- [x] reviewed — code re-read post-fmt; clippy &Path fix applied

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — observe_session_marker called in BOTH run() and run_with_storage() (the production binary uses run_with_storage; grep confirms both call sites); switch_meta rides parts.metadata into the ingested episode; log_switch_line verified by the e2e via real binary stderr
- [x] DEAD-CODE (code) — read_active_at is used by tests now and is the documented read-side contract for scratchpad-handover (task 2); every other new symbol referenced in production paths
- [x] SEMANTIC — sessions.json shape documented in the module header matches the serde structs; HOOK-06 discipline statement verified against every error path (all degrade to warn)

### GATE RECORD
Outcome: PASS  (auto-resolved under autonomy:auto — red/green evidence complete, e2e proves the binary-level contract, failure paths all tested)
Reviewed by: AI (auto-gate) · date: 2026-06-11

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): the stderr "session_switch" line frequency vs SessionStart count (a switch on EVERY start would mean marker writes are failing silently); warn-line rate for sessions-file IO.
Spec delta for the next loop: capture ONE real Claude Code SessionEnd envelope to confirm the docs-typed payload (the freeze flag); scratchpad-handover consumes read_active_at + the stderr line.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
