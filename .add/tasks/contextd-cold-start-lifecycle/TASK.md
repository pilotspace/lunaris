# TASK: Contextd cold-start lifecycle + verify cleanup

slug: contextd-cold-start-lifecycle · created: 2026-07-14 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it, or run `add.py autonomy set`. -->
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower the
     autonomy level to `manual` or `conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures): `scripts/lunaris-codex-hook-adapter.py:contextd_request` (deadline computed BEFORE start_contextd; 300ms default eats spawn+GGUF-load → first-prompt returns {} silently); `:start_contextd` (unlinks the socket of a LIVE mid-load daemon and spawns a duplicate — the restart storm; 5 leaked daemons observed from 2 verify runs); `:spawn_contextd`, `:wait_for_contextd(250ms)`, `:prune_contextd_duplicates` (per-socket only, pattern anchored on binary path — works); `scripts/setup-lunaris-agents.py:stop_verify_contextd` (pgrep -f "--socket …" — pattern starts with `--`, pgrep parses it as a FLAG and errors; check=False swallows → cleanup is a silent no-op); `:run_verify` (inject retries 10×500ms, each with the 300ms deadline).
Context (working folder): findings in memory `project_lunaris_hooks_deep_test_findings` §1–2; live evidence: default-env verify FAILED at inject with last output '' while contextd found the marker instantly once warm; LUNARIS_CONTEXT_TIMEOUT_MS=15000 → PASS; 7 stray daemons killed by hand.
Honors (patterns / conventions): stdlib-only scripts; stage-labeled "VERIFY FAIL: <stage>" contract (test_turnkey_verify pins it); errors visible not swallowed (design-for-failure).
Anchors the contract cites: `contextd_request`, `start_contextd`, `stop_verify_contextd`, `LUNARIS_CONTEXT_COLD_TIMEOUT_MS` (new), verify `cleanup` stage (new).

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: contextd cold-start survives its own model load; verify cleans up after itself.
Framings weighed: adapter-side lifecycle fixes (chosen) · Rust-side async health-during-load (bigger, defer) · document-the-override (ships nothing).
Must:
<must>
  - contextd_request: when THIS call spawned the daemon (cold start), the request deadline extends to LUNARIS_CONTEXT_COLD_TIMEOUT_MS (default 15000ms); a warm daemon keeps the caller's timeout_ms unchanged
  - start_contextd: never unlink a socket whose owning contextd process is still alive (mid-load daemon) — no duplicate spawn, no storm
  - stop_verify_contextd: actually terminates the daemon(s) bound to the verify socket (TERM, grace, then KILL) — pgrep pattern must not start with '-'
  - run_verify passes end-to-end with DEFAULT env (no LUNARIS_CONTEXT_TIMEOUT_MS override) and leaves ZERO contextd processes on its private socket
</must>
Reject:
<reject>
  - verify leaving a live daemon on its socket -> "VERIFY FAIL: cleanup"
</reject>
After:
<after>
  - first prompt after boot gets injected memories (cold GGUF load rides inside the extended first-call deadline)
  - repeat verifies do not accumulate GGUF-loaded daemons
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ 15s default cold budget covers the GGUF load on target boxes — lowest confidence because load time varies with disk/RAM pressure (this box ≈ several s); if wrong: first call still degrades (but warm calls recover) — env-tunable, cost bounded.
  - [x] contextd serves health only when not blocked mid-request — confirmed by the observed storm (dupes spawned during load).
  - [x] pgrep -f with a leading-dash pattern errors — confirmed: `pgrep -f "--socket x"` exits 2 (usage).
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: stop_verify_contextd kills the socket's daemon
  Given a process whose argv contains "--socket <path>" is alive
  When stop_verify_contextd(<path>) runs
  Then the process is dead within the grace window
  And unrelated processes are untouched

Scenario: live socket is never unlinked
  Given a socket file whose owner process (argv contains the socket path) is alive but not answering health
  When start_contextd(<path>) runs
  Then the socket file still exists
  And no duplicate daemon was spawned

Scenario: cold start extends the deadline; warm does not
  Given no daemon on the socket and a cold budget of 15000ms
  When contextd_request spawns the daemon and the first reply takes seconds
  Then the request succeeds within the cold budget
  And a request against an already-warm daemon still honors the caller's timeout_ms

Scenario: default-env verify passes and cleans up (live)
  Given installed hooks, live Moon, and NO timeout overrides in the environment
  When setup-lunaris-agents.py --verify runs
  Then it prints both VERIFY PASS lines and exits 0
  And pgrep finds no contextd bound to the verify socket afterwards
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
contextd_request(request, timeout_ms, autostart=True)
  cold (this call spawned):  deadline = max(timeout_ms, LUNARIS_CONTEXT_COLD_TIMEOUT_MS | 15000)
  warm (daemon pre-existing): deadline = timeout_ms   (unchanged)
start_contextd(socket_path)
  socket exists AND a live process's argv contains str(socket_path) -> return (no unlink, no spawn)
  else -> unchanged (lock, prune, spawn, wait)
stop_verify_contextd(socket_path)   [setup-lunaris-agents.py]
  pgrep -f <pattern-not-starting-with-dash> -> SIGTERM all -> grace ≤2s -> SIGKILL survivors
run_verify: after cleanup, any surviving daemon on the verify socket -> "VERIFY FAIL: cleanup" exit 1;
  no env overrides required for PASS
Env: LUNARIS_CONTEXT_COLD_TIMEOUT_MS (new, default 15000) — documented in docs/integration/claude-code.md
```

Least-sure flag surfaced at freeze: [test] the live default-env verify discriminator depends on
this box's GGUF load fitting the 15s default — a slower box would need the env raise (cost: flaky
gate on other machines; mitigated: test is env-gated to boxes with Moon + weights). [contract]
liveness check keys on argv containing the socket path — a foreign process embedding the same
string would suppress autostart (accepted: paths are per-run tmpdirs).

Status: FROZEN @ v1 — approved by Tin Dang (delegated fully-auto, standing "keep going" 2026-07-14)

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every scenario; live verify discriminator env-gated (LUNARIS_VERIFY_LIVE=1).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_stop_verify_contextd_kills_socket_daemon: dummy process with "--socket <tmp>" argv → stop → dead ≤2s (RED: pgrep flag-parse no-op)
  - test_start_contextd_never_unlinks_live_socket: socket file + live dummy owner + monkeypatched spawn → start_contextd → file exists, spawn not called (RED: unlinked today)
  - test_cold_request_extends_deadline_warm_does_not: fake slow contextd (health instant, recall delayed 2.5s); warm path with timeout 300 → {}; cold path (adapter spawned marker) → success (RED: cold path returns {})
  - test_default_env_verify_passes_and_cleans_up (live, gated): run setup+verify with overrides STRIPPED from env → both PASS lines + exit 0 + no daemon on socket (RED: inject fails with default env)
</test_plan>

Tests live in: `scripts/tests/test_contextd_lifecycle.py` · MUST run red (missing implementation) before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `scripts/lunaris-codex-hook-adapter.py` `scripts/setup-lunaris-agents.py` `scripts/tests/test_contextd_lifecycle.py` `docs/integration/claude-code.md`
Strategy (ordered batches): 1. stop_verify_contextd fix (+kill test green) 2. start_contextd liveness guard 3. cold-deadline in contextd_request 4. verify cleanup stage + docs 5. live default-env verify green.
Safety rule (feature-specific): warm-path timeout semantics byte-identical; never SIGKILL before TERM+grace; cleanup only targets processes whose argv contains the verify-private socket path.
Code lives in: `scripts/`
Constraints: do NOT change any test or the contract; stdlib only; ask if unclear.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — test_contextd_lifecycle 3/3 unit + live default-env discriminator (LUNARIS_VERIFY_LIVE=1, Moon 6399): both PASS lines, exit 0, ZERO daemons on the verify socket; turnkey regression 4/4
- [x] coverage did not decrease — 4 new tests, none removed
- [x] no test or contract was altered during build
- [x] the green was EARNED — kill test red (pgrep flag-parse), unlink test red (socket unlinked), cold-deadline red (cold path returned {}); each observed before the fix
- [x] concurrency / timing — flock-serialized start path unchanged; liveness guard runs under the same lock; TERM→2s grace→KILL never skips the grace
- [x] no exposed secrets / injection — pgrep patterns are tmpdir paths (no user input); no new deps
- [x] layering — adapter + setup script only; Rust untouched
- [x] reviewed — self-review + live end-to-end proof (delegated fully-auto)

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
- [x] default-env verify (no timeout overrides) prints both PASS lines and exits 0 — confirmed live on Moon 6399
- [x] no contextd process survives a verify run — confirmed: pgrep leak assertion in the live test + verify's own cleanup stage
- [x] warm-path timeout semantics unchanged — confirmed: warm leg of the cold-deadline test pins 300ms → {}

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — socket_owner_alive called by start_contextd; verify_socket_daemons by run_verify cleanup stage; cold budget read in contextd_request via env_int_any
- [x] DEAD-CODE (code) — no orphans; stop_verify_contextd rewritten in place
- [x] SEMANTIC (docs) — claude-code.md turnkey section read in full; cleanup stage + cold budget documented

### GATE RECORD
Outcome: PASS
Reviewed by: Claude (delegated fully-auto by Tin Dang) · date: 2026-07-14

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): count of lunaris-contextd processes over time on dev boxes; first-prompt-of-session injection hit-rate.

### Spec delta
- [SPEC · open] contextd should answer health during model load (Rust-side async load or a "loading" status) — the Python liveness guard is a workaround for a daemon that blocks its accept loop (evidence: storm only exists because health goes dark mid-load)
- [SPEC · open] adapter errors are swallowed unless LUNARIS_CODEX_HOOK_DEBUG=1 — degrade reasons should reach a log file by default (evidence: '' output indistinguishable from no-memories during the deep test)
- [SPEC · open] idle-exit for contextd — a daemon that served nothing for N hours should exit and free model memory (evidence: leaked daemons held GGUF RAM for hours)

### Competency deltas
- [TDD · open] argv-marker dummy processes make pgrep-based lifecycle code unit-testable without the real binary (evidence: kill + liveness tests run in ms)
- [ADD · open] a leading-dash pgrep pattern is a silent no-op with check=False — lint-worthy pattern for shell-out code (evidence: 5 leaked daemons)
