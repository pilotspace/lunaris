# TASK: Curation extracts prompt field from captured prompt events

slug: curation-prompt-snippet · created: 2026-07-14 · stage: production
autonomy: auto
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> One file = one task.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-hook/src/context.rs:1068-1130` — `summarize_memory_json(source, value)`: extracts path/new_string/output/command but has NO `prompt` branch → captured UserPromptSubmit envelopes (keys: cwd, hook_event_name, permission_mode, prompt, wrapped in codex_payload) summarize to None → lossy fallback renders raw smart-quote-sanitized envelope JSON, 260-char cap truncates before the payload.
- Live repro (2026-07-14 full Claude Code test): session A prompt "…crimson beacon marker is XR-9913…" retrieved as TOP hit in session B, but additionalContext snippet = `{ " codex_hook_event_name " : " UserPromptSubmit " , … " prompt " : " Remember [truncated]` — marker never surfaces; real cross-session prompt recall is retrieval-good, render-broken.
- `crates/lunaris-hook/src/context.rs:1149` — `string_field` (trim-tolerant) + `object_field` (added today) already resolve the sanitized keys.
- The turnkey verify envelope (setup-lunaris-agents.py) works around this by copying output to top level — tool events summarize; PROMPT events are the uncovered class.
Honors: curation snippet cap 260 chars; summaries must not render raw JSON braces (existing test asserts `!snippet.contains('{')`).
Anchors the contract cites: `summarize_memory_json`, `string_field`, `trim_to_chars`

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: prompt-event curation — captured prompts summarize to `prompt: <text>`
Must:
<must>
  - `summarize_memory_json` extracts a `prompt` string field (trim-tolerant key) and returns `prompt: {trim_to_chars(prompt, 220)}` when no tool path/output/command matched.
  - Works through the codex_payload wrapper (session-A envelope shape) and at top level.
</must>
Reject:
<reject>
  - Envelope with tool output AND prompt-like text -> tool summary still wins (prompt branch is LAST, after output/command)
</reject>
After:
<after>
  - Session-B inject snippet for a recalled prompt episode reads `prompt: Remember this…XR-9913…` — payload inside the snippet cap.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ 220 chars of prompt text is enough for typical marker recall — if wrong: long prompts still truncate their tail (cost: same as today, never worse).
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: sanitized prompt envelope summarizes
  Given a smart-quote space-padded envelope with codex_payload.prompt = "the crimson beacon marker is XR-9913"
  When curate_context_memories runs
  Then the snippet starts with "prompt: " and contains "XR-9913" and no "{"

Scenario: tool output still wins over prompt
  Given an object with both output="deploy ok" and prompt="ignore me"
  When summarize runs
  Then the snippet is "tool output: deploy ok"
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
crates/lunaris-hook/src/context.rs :: summarize_memory_json
  after the `command` branch, before the final None:
    if let Some(prompt) = string_field(object, &["prompt"]) {
        return Some(format!("prompt: {}", trim_to_chars(prompt, 220)));
    }
```

Status: FROZEN @ v1 — approved by standing fully-auto delegation (live-repro'd during the user-requested full Claude Code test).
Least-sure flag surfaced at freeze: [contract] none material — single additive branch at the lowest priority; biggest risk is the 220-char tail truncation noted in §1.

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: both scenarios
<test_plan>
  - curation_summarizes_prompt_capture_envelope: sanitized codex_payload.prompt fixture → snippet "prompt: …XR-9913…", no brace
  - tool output precedence asserted in the same test module (existing rich-payload test already covers output extraction; add explicit both-fields object)
</test_plan>

Tests live in: `crates/lunaris-hook/src/context.rs` (unit mod) · MUST run red before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-hook/src/context.rs`
Strategy: 1. red test  2. prompt branch
Safety rule: prompt branch is LAST (after path/output/command) so tool summaries keep precedence.
Constraints: do NOT change any test or the contract.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — lunaris-hook lib 19/19 (incl. both new tests)
- [x] coverage did not decrease — 2 tests added
- [x] no test or contract altered during build
- [x] green EARNED — red-first (prompt test failed on raw-JSON fallback); live proof through real Claude Code + adapter
- [x] concurrency safe — pure additive branch
- [x] no secrets/injection/deps — snippet proof shows REDACTED, raw secrets absent
- [x] layering follows conventions — single function, clippy/fmt clean
- [x] reviewed — self-review under standing fully-auto delegation

### Build expectations — confirmed at gate
- [x] LIVE: contextd restarted on the rebuilt binary; real Claude Code session B received the marker in lunaris_memory_context (model acknowledged the content; declined to assert it as fact — model-side prudence about injected context, not a render failure; noted as §7 delta on context framing)
- [x] adapter inject additionalContext = `prompt: Remember this for later sessions: the crimson beacon marker is XR-9913 and it lives behind the jade proxy on port 5252…` — marker + port inside the cap, `REDACTED` in place of the credential

### Deep checks
- [x] WIRING — reached via summarize_codex_payload → summarize_memory_json on the real session-A envelope (live snippet proves it)
- [x] DEAD-CODE — none
- [x] SEMANTIC — n/a

### GATE RECORD
Outcome: PASS
Reviewed by: Claude (auto-gate, live evidence above) · date: 2026-07-14

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch: prompt-recall hit-rate in injected context

### Spec delta
- [SPEC · open] injected lunaris_memory_context framing: a cautious model treats hook-injected memory as untrusted "planted" content and may refuse to answer from it — consider provenance wording ("stored by you in a previous session on <date>") to earn trust (evidence: live session B refusal 2026-07-14)

### Competency deltas
