# TASK: Signal-gate hook captures so only high-value tool events become durable memories

slug: capture-signal-gate · created: 2026-07-14 · stage: production
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

Touches (files · symbols · signatures):
- `scripts/lunaris-codex-hook-adapter.py:run_capture` (lines 99-142) — the capture entry.
  Fast-path (100-123) unconditionally sends `"payload": event` (the WHOLE hook envelope:
  cwd, duration_ms, effort, permission_mode, prompt_id + any tool content) to contextd
  `capture_tool_call`/`capture_tool_result` for every pre/posttooluse event. This is the
  noise source: contextd `spawn_capture_tool` stores the payload raw (no gate, no curate).
- `scripts/lunaris-codex-hook-adapter.py:compact_kind` (539) — normalizes to `pretooluse`/
  `posttooluse`; `extract_tool_name` (578); `extract_paths` (recursive path harvest);
  `LOW_VALUE_TOOLS = {"pwd","date","ls"}` (43, currently only gates post-tool *injection*).
Context (working folder): `scripts/tests/test_hooks_merge_safe.py` is the adapter-test
  precedent (import-by-path via importlib, pure-Python, no contextd). New test sits beside it.
Honors (patterns / conventions): design-for-failure (gate is fail-OPEN — any uncertainty
  captures, never drops silently on parse error); pure-function gate (no IO) so it is unit-testable
  without a live daemon; TDD red/green.
Anchors the contract cites: `capture_has_signal(event: dict) -> bool` (new pure predicate) and
  `run_capture` (its single call site, applied before the contextd send).

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: capture signal-gate — a pure predicate `capture_has_signal(event)` that `run_capture`
consults before sending an event to contextd, so contentless / low-value hook events never
become durable memories.
Framings weighed: content-predicate gate (chosen — keep an event iff it carries real content:
a touched path, or a non-empty content field) · event-kind blocklist (rejected — dropping all
`pretooluse` is too coarse; an Edit's pretooluse carries the diff) · curate-at-capture (deferred
to a follow-up task — orthogonal concern; this task only decides keep-vs-drop, not payload shape).
Must:
<must>
  - `capture_has_signal(event)` returns True when the event carries a touched path
    (`extract_paths(event)` non-empty) OR any non-empty content field among
    {error, stderr, output, result, tool_response, toolResponse, tool_input, toolInput,
    message, content, prompt, command, patch, new_string, diff}.
  - Returns False when the tool name (lowercased) is in `LOW_VALUE_TOOLS` ({pwd, date, ls}).
  - Returns False when the event is metadata-only — no path and no non-empty content field
    (e.g. {cwd, duration_ms, permission_mode, prompt_id} envelope with no tool body).
  - `run_capture` skips the contextd send (returns 0, success) when the gate returns False,
    for BOTH the fast-path and the subprocess path.
  - Fail-OPEN: any malformed / non-dict event, or a gate exception, captures (returns True) —
    never drop on uncertainty.
  - Gate is bypassable via env `LUNARIS_CONTEXT_CAPTURE_GATE=off` (default on) so an operator
    can restore capture-everything without a redeploy.
</must>
Reject:
<reject>
  - metadata-only envelope (no path, no content field) -> gate False -> "skipped_no_signal"
  - tool in LOW_VALUE_TOOLS -> gate False -> "skipped_low_value_tool"
</reject>
After:
<after>
  - A metadata-only or low-value event produces NO contextd request (0 episodes minted).
  - An event with a real path or content field is captured exactly as today (unchanged path).
  - `run_capture` still returns 0 on a skip (a skip is success, not an error).
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Real Claude Code PostToolUse events always carry tool_input/tool_response, so the gate rarely
    fires on THEM — lowest confidence because the noise the user saw is the raw ENVELOPE of real
    events, which the gate keeps (it only drops contentless ones). If wrong (many real events are
    contentless): gate over-drops and recall loses signal. Cost: mitigated by fail-OPEN + the
    `=off` kill-switch; curate-at-capture (follow-up) is the real fix for envelope-render noise.
  - [x] `LOW_VALUE_TOOLS` is safe to reuse for capture-drop — confirmed: it is {pwd,date,ls},
    all genuinely valueless to recall.
  - [x] contextd treats "no request sent" identically to today's "sent then stored" minus the
    row — confirmed: capture is best-effort/fire-and-forget, no ack consumed.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: metadata-only envelope is dropped
  Given a posttooluse event with only {cwd, duration_ms, permission_mode, prompt_id}
  When capture_has_signal(event) is evaluated
  Then it returns False
  And run_capture sends NO contextd request and returns 0

Scenario: edit event with a touched path is kept
  Given a posttooluse event whose tool_input carries file_path + new_string
  When capture_has_signal(event) is evaluated
  Then it returns True
  And run_capture proceeds to send the capture (unchanged behavior)

Scenario: bash command output is kept
  Given a posttooluse event with tool "Bash" and a non-empty tool_response
  When capture_has_signal(event) is evaluated
  Then it returns True

Scenario: low-value tool is dropped
  Given a posttooluse event with tool "pwd" and a stdout string
  When capture_has_signal(event) is evaluated
  Then it returns False
  And run_capture sends NO contextd request and returns 0

Scenario: malformed event fails open
  Given an event that is not a dict (e.g. a bare string) or is missing expected keys
  When capture_has_signal(event) is evaluated
  Then it returns True   # fail-open: never drop on uncertainty

Scenario: kill-switch restores capture-everything
  Given LUNARIS_CONTEXT_CAPTURE_GATE=off and a metadata-only event
  When run_capture evaluates the gate
  Then the gate is bypassed and the capture is sent (returns True path)
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
capture_has_signal(event: Any) -> bool          # pure, no IO, fail-open
  True  when: extract_paths(event) non-empty
              OR any of {error,stderr,output,result,tool_response,toolResponse,
                         tool_input,toolInput,message,content,prompt,command,
                         patch,new_string,diff} is a non-empty str/dict/list
              OR event is not a dict (fail-open)
  False when: extract_tool_name(event).lower() in LOW_VALUE_TOOLS
              OR (dict event AND no path AND no non-empty content field)

run_capture(event) -> int                        # existing symbol, gated
  when gate is ON (LUNARIS_CONTEXT_CAPTURE_GATE != "off")
    and capture_has_signal(event) is False -> return 0  (no contextd send)
  otherwise -> unchanged (fast-path or subprocess capture)

Env: LUNARIS_CONTEXT_CAPTURE_GATE = on|off   (default on)
Schema: no storage schema change — this only decides whether a capture request is emitted.
```

Status: FROZEN @ v1 — approved by Tin Dang (standing fully-auto delegation; contained
Python gate, fail-open + kill-switch, no schema change, non-security/mechanical scope).

Least-sure flag surfaced at freeze: [spec] the gate keeps the raw envelope for real events (it
only drops contentless / low-value ones), so it reduces episode COUNT, not per-episode render
size — the envelope-render noise is fixed by the deferred curate-at-capture follow-up. Why it
might be wrong: if most real PostToolUse events are contentless, the gate over-drops and recall
loses signal. Cost if wrong: mitigated by fail-OPEN + the `LUNARIS_CONTEXT_CAPTURE_GATE=off`
kill-switch. Surfaced; accepted for this cut.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: 100% of gate branches (path-hit, content-hit, low-value, metadata-only,
non-dict fail-open) + the run_capture skip + the kill-switch.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_metadata_only_event_is_dropped: gate({cwd,duration_ms,...}) is False
  - test_edit_event_with_path_is_kept: gate(edit w/ file_path+new_string) is True
  - test_bash_output_is_kept: gate(Bash w/ tool_response) is True
  - test_low_value_tool_is_dropped: gate(pwd w/ stdout) is False
  - test_malformed_event_fails_open: gate("not-a-dict") is True; gate({}) is True-or-False-but-no-raise
  - test_run_capture_skips_on_no_signal: monkeypatch contextd_request to record calls;
    run_capture(metadata_only) makes 0 calls and returns 0
  - test_run_capture_sends_on_signal: run_capture(edit_event) makes >=1 contextd call
  - test_kill_switch_bypasses_gate: env LUNARIS_CONTEXT_CAPTURE_GATE=off -> run_capture(metadata_only)
    makes a contextd call
</test_plan>

Tests live in: `scripts/tests/test_capture_signal_gate.py` · MUST run red (missing
`capture_has_signal` / ungated `run_capture`) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `scripts/lunaris-codex-hook-adapter.py` `scripts/tests/test_capture_signal_gate.py`
Strategy (ordered batches): 1. write red tests (test_capture_signal_gate.py). 2. add
`capture_has_signal(event)` pure predicate + `_capture_gate_enabled()` env reader. 3. gate both
run_capture paths (fast-path before the two contextd sends; subprocess path before the subprocess).
Safety rule (feature-specific): fail-OPEN — the gate is wrapped so any exception / non-dict returns
True (capture); a skip must still return 0 (success), never a non-zero hook exit that blocks the tool.
Code lives in: `scripts/lunaris-codex-hook-adapter.py`
Constraints: do NOT change any test or the contract; allow-list packages only (stdlib only); ask if unclear.

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
- [x] metadata-only event emits 0 contextd requests — confirmed by test_run_capture_skips_on_no_signal (monkeypatched contextd_request call-count == 0) GREEN
- [x] real edit/bash event still captured — confirmed by test_run_capture_sends_on_signal (call-count >= 1) GREEN
- [x] kill-switch restores capture-everything — confirmed by test_kill_switch_bypasses_gate GREEN
- [x] fail-open holds — confirmed by test_malformed_event_fails_open (no raise, returns True for non-dict) GREEN

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `capture_has_signal` + `_capture_gate_enabled` are called at the top of
  `run_capture` (its single entry, gating BOTH the fast-path and subprocess path); `CAPTURE_CONTENT_FIELDS`
  is consumed by `capture_has_signal`. Confirmed by grep + green run_capture tests.
- [x] DEAD-CODE (code) — no orphaned symbol; every new name is referenced. `extract_paths`/`extract_tool_name`/
  `LOW_VALUE_TOOLS` are reused (pre-existing).
- [x] SEMANTIC — verified `cwd` exclusion (`scan` strips top-level cwd) is required because
  `extract_paths` harvests `cwd` keys as paths; without it every metadata-only envelope would
  falsely read as signal (the metadata-only test would fail). Confirmed by test_metadata_only_event_is_dropped GREEN.

### GATE RECORD
Outcome: PASS
Evidence: 8/8 new tests GREEN (test_capture_signal_gate.py); 4/4 sibling regression GREEN
(test_hooks_merge_safe.py); adapter parses (ast.parse OK). No test or contract altered during build.
Green earned: the metadata-only drop is the discriminating case — pre-build run_capture demonstrably
SENT the metadata-only envelope (red failure captured), post-build it emits 0 requests. Fail-open +
kill-switch both exercised. No secrets/injection surface (pure predicate, stdlib only).
Reviewed by: Tin Dang (standing fully-auto delegation) · date: 2026-07-14

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
