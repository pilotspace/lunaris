# TASK: memory.recall returns LLM-optimized curated snippets (raw opt-out)

slug: recall-llm-snippets · created: 2026-07-14 · stage: production
autonomy: auto
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> One file = one task.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-mcp/src/tools/recall.rs:186` — hit mapping is a raw 200-char truncate of stored text; hook/decision envelopes surface as smart-quoted JSON noise (live proof this session: recall hits read `{ " decision " : " Deep-test decision…`). RecallParams (32-47, deny_unknown_fields) has no raw toggle; RecallHit.content doc says "≤ 200 chars".
- `crates/lunaris-hook/src/context.rs:1053-1181` — the PROVEN curation stack: summarize_memory_for_context, parse_jsonish (smart-quote normalize), summarize_memory_json (decision:/edit:/tool/prompt branches), summarize_codex_payload, object_field/string_field (trim-tolerant), is_low_value_text (context-only policy), trim_to_chars (1210), single_line (1217).
- `crates/lunaris-core/` — both lunaris-hook and lunaris-mcp depend on lunaris-core; core has serde_json. Precedent: shared helpers live in core (keyspace RC-1, scope_resolver).
- Owner decision (2026-07-14 AskUserQuestion): curated content by default + `raw: true` param returns the old raw preview.
Honors: token economy for LLM consumers is the point — hits are read by models; MCP DTO discipline (deny_unknown_fields); hook curation tests pin exact behavior and MUST stay green through the extraction.
Anchors the contract cites: `lunaris_core::snippet` (new module), `snippet::summarize`, `snippet::parse_jsonish`, `snippet::summarize_json`, `snippet::trim_to_chars`, `snippet::single_line`, `RecallParams.raw`, `RecallHit.content`

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: curated LLM-ready recall snippets, shared render module
Framings weighed: extract rendering to lunaris-core::snippet, both crates consume (chosen) · duplicate ~110 lines into lunaris-mcp (rejected: DRY, drift) · mcp depends on lunaris-hook (rejected: layering inversion)
Must:
<must>
  - New `lunaris_core::snippet` module: parse_jsonish, summarize_json (all source-aware branches incl. codex_payload/tool_input/tool_response trim-tolerant lookups, decision/edit/tool/command/prompt), summarize (parse→summarize_json; None for non-JSON), trim_to_chars, single_line — verbatim behavior from lunaris-hook.
  - lunaris-hook context.rs delegates to the core module (local copies deleted); is_low_value_text and context-only policy stay in hook; ALL existing hook curation unit tests pass unchanged.
  - memory.recall: content = trim_to_chars(summarize(source, text) OR single_line(text), 260) by default; `raw: true` param restores the old 200-char raw truncate.
  - RecallParams gains `#[serde(default)] raw: bool` (deny_unknown_fields kept); RecallHit.content doc + main.rs tool description updated.
</must>
Reject:
<reject>
  - unknown extra field in recall params -> still rejected (deny_unknown_fields regression-pinned by existing tests)
</reject>
After:
<after>
  - A decision hit reads `decision: …; rationale: …` (~60-80% fewer tokens than the raw envelope); raw payloads remain reachable for debugging via raw:true.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Curated cap 260 chars (contextd parity) vs documented 200 — consumers relying on ≤200 could see up to 260; cost: negligible (soft prose contract), docs updated.
  - [x] is_low_value_text must NOT move to core — it is prompt-context policy (drops noise from injection), wrong for explicit recall where the user asked.
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: decision episode renders curated
  Given an episode source='decision:x' content='{"decision":"adopt launchd","rationale":"KeepAlive restarts"}'
  When memory.recall runs (default)
  Then the hit content is 'decision: adopt launchd; rationale: KeepAlive restarts' and contains no '{'

Scenario: smart-quote envelope renders prompt summary
  Given a stored sanitized envelope { " codex_payload " :{ " prompt " : " marker XR-1 " }}
  When snippet::summarize runs
  Then it returns 'prompt: marker XR-1'

Scenario: raw opt-out returns stored bytes
  Given the same decision episode
  When memory.recall runs with raw=true
  Then the hit content is the raw 200-char truncate (contains '{')

Scenario: plain text passes through
  Given an episode with non-JSON content 'The cobalt gateway is CG-1.'
  When memory.recall runs (default)
  Then the hit content is the single-line text unchanged

Scenario: hook curation unchanged
  Given the existing lunaris-hook curation unit tests
  When the suite runs after the extraction
  Then all pass unchanged
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
crates/lunaris-core/src/snippet.rs  (new, pub)
  pub fn summarize(source: &str, text: &str) -> Option<String>   // JSON-derived summary; None for non-JSON/unrecognized
  pub fn summarize_json(source: &str, value: &serde_json::Value) -> Option<String>
  pub fn parse_jsonish(text: &str) -> Option<serde_json::Value>
  pub fn trim_to_chars(text: &str, max_chars: usize) -> String
  pub fn single_line(text: &str) -> String

crates/lunaris-hook/src/context.rs
  summarize_memory_for_context/body helpers -> delegate to lunaris_core::snippet (behavior identical)

crates/lunaris-mcp/src/tools/recall.rs
  RecallParams += #[serde(default)] raw: bool
  content(default) = trim_to_chars(&summarize(source,text).unwrap_or_else(|| single_line(text)), 260)
  content(raw)     = text.chars().take(200)   // unchanged legacy
```

Status: FROZEN @ v1 — approved by Tin Dang (AskUserQuestion 2026-07-14: "Curated by default + raw param").
Least-sure flag surfaced at freeze: [contract] the 200→260 cap drift on curated content — surfaced in §1 assumptions; accepted (docs updated, soft contract).

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every scenario
<test_plan>
  - lunaris-core snippet unit mod: decision render · smart-quote codex prompt render · non-JSON returns None · trim/single_line pins
  - lunaris-mcp recall.rs test mod: recall_curates_decision_snippet (memory:// engine, no '{', starts 'decision: ') · recall_raw_param_returns_stored_bytes · recall_plain_text_passthrough
  - regression: existing hook curation tests + existing recall tests (constructor gains raw: false — mechanical field add, not weakening)
</test_plan>

Tests live in: `crates/lunaris-core/src/snippet.rs` (unit mod) + `crates/lunaris-mcp/src/tools/recall.rs` (unit mod) · MUST run red before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-core/src/snippet.rs` `crates/lunaris-core/src/lib.rs` `crates/lunaris-hook/src/context.rs` `crates/lunaris-mcp/src/tools/recall.rs` `crates/lunaris-mcp/src/main.rs` `crates/lunaris-mcp/src/embedded_moon.rs`
<!-- scope amended at verify attempt 1: embedded_moon.rs holds one RecallParams constructor — the mechanical raw:false addition declared exempt in Constraints below required touching it; forgot to list it here at freeze. -->

Strategy: 1. core module + red tests  2. mcp wiring + red tests  3. hook delegation  4. docs/tool description
Safety rule: hook curation behavior byte-identical (its unit tests are the oracle); deny_unknown_fields never dropped.
Constraints: do NOT change any test or the contract (mechanical raw:false constructor additions exempt, declared here).

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — core 78/78 (snippet 4 new), mcp bin 62/62 (3 new recall tests), hook lib 19/19 (curation oracle 6/6 unchanged), server_boot 1/1; workspace sweep 96 suites ok. ONE pre-existing failure: `lunaris-mcp tests/cold_start.rs` wall-clock budget (740ms > 500ms) — fails identically at unmodified HEAD (725ms, proven via stash), host-load flake, untouched by this diff.
- [x] coverage did not decrease — +7 tests net; zero removed
- [x] no test or contract altered during build — only declared-exempt mechanical `raw: false` additions to 5 existing constructors (4 recall.rs + 1 embedded_moon.rs)
- [x] green EARNED — red confirmed first (todo!() panics ×4; E0560 no-field-raw ×3)
- [x] concurrency safe — pure functions, no locks/await/state
- [x] no secrets/injection/deps — no new crate deps (core already had serde_json); snippet render happens AFTER scrub-at-ingest, raw path unchanged
- [x] layering ok — helpers moved DOWN into lunaris-core (RC-1 keyspace precedent); hook + mcp both consume; no hook→mcp or mcp→hook edge
- [x] reviewed — full diff re-read at gate

### Build expectations — confirm at gate
- [x] LIVE (2026-07-14): fresh release lunaris-mcp stdio vs Moon 6381 scope git_3956f48ad8e2696b → top hit `decision: Shipped milestone claude-code-flagship as PR #51 …; rationale: Tin chose 'Branch + PR now' …`; raw:true returns `{ " decision " : " Shipped…` envelope bytes. (source_prefix-filtered leg returns 0 on Moon — pre-existing moon-hybrid-filter-bypass follow-up, upstream of the mapping this task touched; same filter test green on embedded backend.)
- [x] token delta: full stored envelope ~911 chars → curated 258 chars (−72%); vs old 200-char raw cap the curated snippet is +58 chars but carries the COMPLETE decision+rationale instead of truncated JSON noise (old preview lost the rationale entirely)
- [x] MCP schema object-rooted — server_boots_and_lists_all_tools green

### Deep checks
- [x] WIRING — recall.rs:200-202 calls `snippet::{trim_to_chars,summarize,single_line}`; context.rs:9 imports `lunaris_core::snippet::{parse_jsonish,single_line,summarize_json,trim_to_chars}`; grep confirms zero local copies remain
- [x] DEAD-CODE — parse_jsonish/summarize_memory_json/summarize_codex_payload/object_field/string_field/trim_to_chars/single_line deleted from context.rs (~120 lines); is_low_value_text/dedupe_key/scrub_and_trim stay hook-side per contract
- [x] SEMANTIC — main.rs recall tool description + server instructions + RecallHit doc all describe curated-by-default ≤260 chars with raw:true opt-out

### GATE RECORD
Outcome: PASS
Reviewed by: Claude (auto-resolved on complete evidence; sensitivity: mechanical/wire-format, no security surface) · date: 2026-07-14

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch: curated-render fallback rate (None → single_line) as a proxy for uncovered envelope classes

### Spec delta

- [open] **Embedded-double-quote envelopes always fall back to raw single_line** — observed on the FIRST dogfood recall (2026-07-14, scope git_c5419ed101f6f35b): a record_decision whose rationale contained `"Curated by default + raw param"` (real double quotes). The ingest sanitizer smart-quotes ALL `"`, and `parse_jsonish`'s blind `“”→"` reversal re-breaks the inner string → invalid JSON → summarize None → single_line fallback (graceful, but un-curated). Pre-existing in the hook path too (same code). Candidate fixes for a follow-up task: escape inner quotes at record_decision/record_edit write time (JSON-encode before sanitize), or a quote-aware reparse that only reverses smart quotes adjacent to JSON structural chars. Improves: TDD (the scenario suite lacked an inner-quote envelope case).
- [open] **recall on a never-ingested scope errors `unknown index`** instead of returning empty hits (Moon hybrid_search surfaces the raw redis error; the WorkingMemory::find unknown-index→Ok(empty) arm from 133c3dc does not cover the recall DSL path). First-run UX bug for fresh scopes. Improves: SDD.

### Competency deltas

- [open] ADD: amending §5 Scope alone does not clear a scope_violation — the engine snapshots scope at build ENTRY; recovery is `phase tests` → advance ×2 → gate. Improves: ADD.
