# TASK: Adapter forwards caller scope to contextd (cross-project isolation)

slug: contextd-scope-forwarding · created: 2026-07-14 · stage: production
autonomy: auto
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
sensitivity: data

> One file = one task.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `scripts/lunaris-codex-hook-adapter.py:99-227` — six contextd socket request builders (capture_tool_call, capture_tool_result ×2, recall_for_prompt, recall_after_tool, turn_feedback): NONE include `scope`.
- `crates/lunaris-hook/src/context.rs:52-95` — every ContextRequest variant already carries `scope: Option<String>`; `resolve_scope` (884) prefers explicit over cwd/env.
- `crates/lunaris-hook/src/scope.rs:109-115` — `LUNARIS_HOOK_SCOPE` env override is read IN THE DAEMON process when no explicit scope arrives.
- Live repro (2026-07-14 value-proof experiment): hook commands carried `LUNARIS_HOOK_SCOPE=cc-value-proof`, but the long-lived contextd (spawned earlier by a different session) inherited `LUNARIS_HOOK_SCOPE=cc-hook-e2e` — 216 tool episodes from the new project landed in the OLD project's scope, and inject recalls searched the old scope (decisions invisible). Prompt captures (non-fast path, per-event lunaris-hook binary with correct env) landed correctly → sessions were SPLIT across two scopes.
Honors: RFC 0001 scope isolation is the product's core partition contract; adapter is stdlib-only python.
Anchors the contract cites: `run_capture`, `run_prompt_injection`, `run_post_tool_injection`, `run_feedback`, `LUNARIS_HOOK_SCOPE`, `strip_none`

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: every contextd socket request carries the CALLER's scope
Must:
<must>
  - All six contextd request payloads include `"scope": os.environ.get("LUNARIS_HOOK_SCOPE")` (dropped by strip_none when unset).
  - When the env var is unset, payloads omit `scope` (daemon falls back to per-request cwd derivation — unchanged).
</must>
Reject:
<reject>
  - Empty-string env value -> treated as unset (no `scope` key), never sent as ""
</reject>
After:
<after>
  - Two sessions with different LUNARIS_HOOK_SCOPE sharing one daemon write and read ONLY their own scopes.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ resolve_scope treats explicit "" as unset already (`!scope.is_empty()` guard confirmed at context.rs:885-889) — belt-and-braces on the adapter side anyway; if wrong: Scope::new("") error, cost one failed capture.
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: every socket payload carries the env scope
  Given LUNARIS_HOOK_SCOPE=proj-a
  When run_capture (pre+post fast), run_prompt_injection, run_post_tool_injection, run_feedback build requests
  Then each contextd payload includes scope='proj-a'

Scenario: unset env omits scope
  Given LUNARIS_HOOK_SCOPE unset (and empty-string treated the same)
  When the same builders run
  Then no payload contains a scope key

Scenario: live cross-scope isolation (gated)
  Given one contextd daemon spawned under LUNARIS_HOOK_SCOPE=proj-a
  When an adapter capture runs with LUNARIS_HOOK_SCOPE=proj-b
  Then the episode lands under scope proj-b, not proj-a
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
scripts/lunaris-codex-hook-adapter.py
  def hook_scope() -> str | None:   # os.environ.get("LUNARIS_HOOK_SCOPE") or None (empty -> None)
  All six contextd request dicts gain "scope": hook_scope() (strip_none drops None).
  turn_feedback + best-effort post-tool capture payloads included.
No Rust changes (ContextRequest already accepts scope; resolve_scope already prefers it).
```

Status: FROZEN @ v1 — approved by standing fully-auto delegation (P0 scope-isolation bug, live-repro'd during the owner-requested value-proof experiment).
Least-sure flag surfaced at freeze: [contract] none material — additive field the daemon already honors; the live gated test is the proof that matters.

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: both unit scenarios + the gated live discriminator
<test_plan>
  - test_all_socket_payloads_carry_env_scope: monkeypatch contextd_request, drive all six builders with env set, assert scope on every payload
  - test_unset_or_empty_env_omits_scope: same with env unset and env=""
  - test_live_cross_scope_isolation (gated LUNARIS_VERIFY_LIVE=1 + LUNARIS_VERIFY_MOON_URL): daemon born scope-a, capture with scope-b -> episode under proj-b only
</test_plan>

Tests live in: `scripts/tests/test_contextd_scope_forwarding.py` · MUST run red before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `scripts/lunaris-codex-hook-adapter.py` `scripts/tests/test_contextd_scope_forwarding.py`
Strategy: 1. red tests  2. hook_scope() + six payload edits
Safety rule: never send scope="" (Scope::new would reject); strip_none handles None.
Constraints: do NOT change any test or the contract; stdlib only.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — new suite 3/3 (live gate ON), lifecycle 4/4, turnkey_verify 4/4, turnkey_moon_only 9/9
- [x] coverage did not decrease — 3 tests added
- [x] no test or contract altered during build (one harness fix pre-red: fake contextd_request returns {} not None, matching the real return contract)
- [x] green EARNED — red-first assertion failure on missing scope; live discriminator spawns a REAL daemon under scope-a and proves a scope-b caller's episode lands in b with a empty
- [x] concurrency safe — pure payload field; daemon side unchanged
- [x] no secrets/injection/deps — stdlib only
- [x] layering ok — adapter-only; Rust protocol already supported the field
- [x] reviewed — self-review under standing fully-auto delegation; sensitivity=data (scope isolation is the RFC 0001 partition contract)

### Build expectations — confirmed at gate
- [x] LIVE cross-scope isolation: daemon born LUNARIS_HOOK_SCOPE=fwd-a-…, adapter capture under fwd-b-… → episode under b, ZERO keys under a (Moon 6381)
- [x] all existing adapter suites green

### Deep checks
- [x] WIRING — 6 payload builders carry "scope": hook_scope(); the best-effort post-tool capture now goes through strip_none so unset env omits the key
- [x] DEAD-CODE — hook_scope() has 6 call sites
- [x] SEMANTIC — n/a

### GATE RECORD
Outcome: PASS
Reviewed by: Claude (auto-gate, live evidence above) · date: 2026-07-14

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch: cross-scope write rate (should be zero); scope field presence in contextd logs

### Spec delta
- [SPEC · open] contextd could log a warn when its own env scope differs from a request's explicit scope (drift telemetry)
- [SPEC · open] value-proof follow-ups: codex:tool_call:post episodes render raw JSON in inject snippets (another curation class); recall-for-prompt with code-shaped prompts returned empty once (FT analysis suspect) — re-test after this fix since scope bleed contaminated those observations

### Competency deltas
