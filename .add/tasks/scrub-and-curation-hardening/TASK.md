# TASK: Scrubber set extension + curation nested-key tolerance

slug: scrub-and-curation-hardening · created: 2026-07-14 · stage: production
autonomy: auto
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
sensitivity: security

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-hook/src/scrub.rs:48-59` — `BUILTIN_RAW: &[(&str, &str)]` — 5 built-ins today: ENV_KEY (UPPERCASE=val), AWS_KEY (AKIA…), GH_TOKEN (gh?_), SSH_KEY (PEM header), JWT (eyJ…). Doc table lines 5-11; "five built-in" prose at 46/73.
- `crates/lunaris-hook/src/scrub.rs:149-161` — `ScrubEngine::apply(&self, &mut String) -> usize` — sequential replace_all, built-ins first.
- `crates/lunaris-hook/src/context.rs:1068-1082` — `summarize_memory_json`: nested lookups `object.get("codex_payload"/"tool_input"/"tool_response")` are EXACT-match; `string_field` (1149-1157) is trim-tolerant (`key.trim() == name`). Smart-quote-scrubbed stored JSON reparses with space-padded keys (`" tool_response "`) → nested lookups miss (open deep-test bug; top-level `output` workaround pinned in setup-lunaris-agents.py verify envelope comment).
- `crates/lunaris-hook/src/context.rs:1190` — `scrub_and_trim(text, max_chars)` — inject-side snippet scrub (defense-in-depth for already-stored secrets), applied at 360/411/429/476.
- `crates/lunaris-hook/src/context.rs:1289-1301` — `curation_tolerates_scrubbed_smart_quote_json` unit test pins the TOP-LEVEL space-padded-key case; no nested-case test exists.
- `crates/lunaris-hook/tests/scrubber_byte_identical.rs` — HOOK-04 CI gate, 11 tests, per-pattern harness (`ScrubEngine::new().apply` + assert `<REDACTED:…>`), header prose says "five".
- `docs/integration/hooks.md:297,325,588` — "five built-in" prose ×3 + pattern table.
Context (working folder): live proof (2026-07-14 deep test finding #3): `sk-ant-…`, lowercase `password=…`, bearer/Slack/GCP tokens hit storage AND injected context VERBATIM.
Honors: HOOK-04 posture — over-redaction preferred over missing a credential (scrub.rs module doc); T-24-02-01 ReDoS — built-ins must be linear-time; scrubber is additive, never configurable off; red/green TDD.
Anchors the contract cites: `BUILTIN_RAW`, `ScrubEngine::apply`, `summarize_memory_json`, `object_field` (new), `string_field`, `scrub_and_trim`

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: widen scrubber built-ins to the proven-leaking secret families; make curation nested-object lookup trim-tolerant
Framings weighed: extend BUILTIN_RAW in place (chosen) · policy-TOML-shipped defaults (rejected: built-ins must be un-configurable-off) · single mega-regex (rejected: auditability + ReDoS review per pattern)
Must:
<must>
  - New built-ins, each linear-time, each with a distinct `<REDACTED:KIND>` tag: API_KEY (`sk-ant-…` and generic `sk-…{16,}`), SLACK_TOKEN (`xox[baprs]-…`), GITLAB_PAT (`glpat-…`), GCP_KEY (`AIza…{35}`), KV_SECRET (case-insensitive `password|passwd|pwd|secret|token|api[_-]?key` followed by `=`/`:` and a ≥4-char value, tolerant of JSON/YAML quoting), BEARER (`(?i)bearer <token≥16>`).
  - Existing 5 built-ins unchanged (byte-identical outputs on the existing fixture suite).
  - `summarize_memory_json` resolves `codex_payload` / `tool_input` / `tool_response` through a trim-tolerant `object_field` helper (mirror of `string_field`), so space-padded smart-quote-scrubbed keys still summarize.
  - Inject-side proof: `scrub_and_trim` redacts an `sk-ant-…` key (already-stored secrets never reach `additionalContext`).
  - Count-prose sweep: scrub.rs doc table + "five built-in" strings in scrub.rs / scrubber_byte_identical.rs / docs/integration/hooks.md updated to the new set.
</must>
Reject:
<reject>
  - `max_tokens: 4096` / word-boundary bleed -> NOT redacted (KV_SECRET anchors on the whole word)
  - `Bearer` as prose ("the bearer of news arrived") -> NOT redacted (requires a token-shaped value ≥16 chars)
</reject>
After:
<after>
  - A captured event containing sk-ant/password=/xoxb/glpat/AIza/bearer secrets stores only `<REDACTED:…>` tags; recall snippets are scrubbed again on the way out.
  - Nested `tool_response` payloads in smart-quote-scrubbed episodes summarize as `tool output: …` (the top-level `output` verify workaround becomes redundant but stays harmless).
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ KV_SECRET false-positive rate on ordinary code/docs (e.g. `secret: true`, `token: value` placeholders) — lowest confidence because the word list is broad; if wrong: over-redaction of benign config in captured context (accepted HOOK-04 posture; value must be ≥4 non-space chars). Cost: readability, never a leak.
  - [x] Generic `sk-[A-Za-z0-9_\-]{16,}` won't eat prose — 16+ unbroken token chars after literal `sk-` is not natural language.
  - [x] No new pattern is super-linear — all are literal-prefix or bounded-class regexes, no nested quantifiers.
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: anthropic-style API key redacted
  Given content "export key sk-ant-api03-AbCd1234EfGh5678IjKl"
  When ScrubEngine::new().apply runs
  Then content contains "<REDACTED:API_KEY>" and not "sk-ant-"

Scenario: generic sk- API key redacted
  Given content "sk-proj-AbCdEfGh12345678IjKl"
  When apply runs
  Then content contains "<REDACTED:API_KEY>"

Scenario: slack token redacted
  Given content "xoxb-FAKEFAKE00-Kp3fq9AbCdEf12345678"
  When apply runs
  Then content contains "<REDACTED:SLACK_TOKEN>"

Scenario: gitlab PAT redacted
  Given content "glpat-AbCdEfGhIjKlMnOpQrSt"
  When apply runs
  Then content contains "<REDACTED:GITLAB_PAT>"

Scenario: GCP API key redacted
  Given content "AIzaSyA1bC2dE3fG4hI5jK6lM7nO8pQ9rS0tU1v"
  When apply runs
  Then content contains "<REDACTED:GCP_KEY>"

Scenario: lowercase password / json token redacted
  Given "password=hunter22" and "\"token\": \"abcd1234\""
  When apply runs
  Then both contain "<REDACTED:KV_SECRET>"

Scenario: bearer header redacted
  Given "Authorization: Bearer AbCdEf123456789012345"
  When apply runs
  Then content contains "<REDACTED:BEARER>"

Scenario: benign token words survive
  Given "max_tokens: 4096" and "the bearer of news arrived"
  When apply runs
  Then content is unchanged

Scenario: existing five built-ins byte-identical
  Given the existing scrubber_byte_identical fixture suite
  When the suite runs
  Then all pre-existing tests pass unchanged

Scenario: nested smart-quote tool_response summarizes
  Given snippet "{ “ tool_response ” : { “ output ” : “ Moon relay ok ” } }"
  When curate_context_memories runs
  Then the snippet becomes "tool output: Moon relay ok"

Scenario: inject-side scrub of stored secret
  Given stored text "the key is sk-ant-api03-AbCd1234EfGh5678IjKl"
  When scrub_and_trim renders the snippet
  Then output contains "<REDACTED:API_KEY>" and not the key
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
crates/lunaris-hook/src/scrub.rs
  BUILTIN_RAW grows 5 -> 11 (order: existing five unchanged, then):
    API_KEY      r"sk-(?:ant-)?[A-Za-z0-9_\-]{16,}"            -> <REDACTED:API_KEY>
    SLACK_TOKEN  r"xox[baprs]-[A-Za-z0-9\-]{10,}"              -> <REDACTED:SLACK_TOKEN>
    GITLAB_PAT   r"glpat-[A-Za-z0-9_\-]{20}"                   -> <REDACTED:GITLAB_PAT>
    GCP_KEY      r"AIza[0-9A-Za-z_\-]{35}"                     -> <REDACTED:GCP_KEY>
    KV_SECRET    (?i)\b(?:password|passwd|pwd|secret|token|api[_-]?key)["']?\s*[=:]\s*["']?[^\s"']{4,}
                                                               -> <REDACTED:KV_SECRET>
    BEARER       r"(?i)\bbearer\s+[A-Za-z0-9\-._~+/]{16,}=*"   -> <REDACTED:BEARER>
  apply() semantics unchanged; module doc table + count prose updated.

crates/lunaris-hook/src/context.rs
  fn object_field<'a>(object: &'a Map<String, Value>, name: &str) -> Option<&'a Value>
    // key.trim() == name, first match wins (mirror of string_field)
  summarize_memory_json: codex_payload / tool_input / tool_response lookups go
  through object_field; behavior otherwise unchanged.

docs/integration/hooks.md: pattern table + "five built-in" prose -> the new set.
```

Status: FROZEN @ v1 — approved by standing fully-auto delegation (Tin Dang; scrubber gap is finding #3 of the 2026-07-14 deep test; task is milestone-planned).
Least-sure flag surfaced at freeze: [contract] KV_SECRET breadth — `token:`/`secret:` values in benign config get redacted; resolved toward breadth per the documented HOOK-04 over-redaction posture (security sensitivity: under-matching is the dangerous direction).

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: one test per new pattern + both rejects + nested-curation + inject-side scrub; existing 11 scrubber tests untouched
Plan:
<test_plan>
  - scrubber_byte_identical.rs: test12_api_key_sk_ant · test13_api_key_sk_generic · test14_slack_token · test15_gitlab_pat · test16_gcp_key · test17_kv_secret_lowercase_and_json · test18_bearer_header · test19_benign_token_words_survive
  - context.rs mod tests: curation_resolves_nested_smart_quote_tool_response (red today — exact-match get misses space-padded key)
  - context.rs mod tests: inject_snippet_scrubs_stored_api_key (scrub_and_trim on sk-ant text)
</test_plan>

Tests live in: `crates/lunaris-hook/tests/scrubber_byte_identical.rs` + `crates/lunaris-hook/src/context.rs` (unit mod) · MUST run red before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-hook/src/scrub.rs` `crates/lunaris-hook/src/context.rs` `crates/lunaris-hook/tests/scrubber_byte_identical.rs` `docs/integration/hooks.md`
Strategy (ordered batches): 1. red tests  2. BUILTIN_RAW + doc table  3. object_field + summarize wiring  4. hooks.md prose
Safety rule (feature-specific): existing five patterns byte-for-byte unchanged; every new regex hand-checked linear-time; scrubber stays additive (no off switch).
Constraints: do NOT change any test or the contract; no new dependencies.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — scrubber_byte_identical 19/19; lunaris-hook lib 17/17; full `cargo test -p lunaris-hook` all suites green
- [x] coverage did not decrease — 10 new tests, none removed
- [x] no test or contract altered during build — only the contracted count-prose comment in the test-file HEADER changed (mandated by §1 Must, no assertion touched)
- [x] the green was EARNED — red-first (7 scrubber + 2 curation tests failed for missing implementation); live proof runs the real release binary, not the test harness
- [x] concurrency / timing safe — pure regex additions + a pure lookup helper; no locks, no async
- [x] no exposed secrets / injection / new deps — the change is itself the secret-redaction hardening; proof scope deleted from Moon 6381 after capture; stray contextd killed
- [x] layering follows conventions — all changes inside lunaris-hook; clippy --workspace --all-targets clean; fmt clean
- [x] reviewed — self-review under standing fully-auto delegation (diff read in full); sensitivity=security noted: this task only ADDS redaction patterns (breadth flag resolved toward over-redaction at freeze)

### Build expectations — confirmed at the gate
- [x] live adapter capture against Moon 6381 (scope lunaris-scrub-proof, rebuilt release binaries, production adapter path): stored episode content = "credentials: <REDACTED:API_KEY> and <REDACTED:KV_SECRET> done" — raw sk-ant key and password=hunter22 ABSENT (hash-field dump read in full)
- [x] `max_tokens: 4096` + "the bearer of news arrived" unchanged — test19 green
- [x] pre-existing 11 scrubber tests green untouched
- [x] nested smart-quote fixture → "tool output: Moon relay ok" — new unit test green

### Deep checks
- [x] WIRING — object_field used by all three nested lookups (codex_payload/tool_input/tool_response); new patterns ship in BUILTIN_RAW compiled by ScrubEngine::new(), the same constructor used by capture (main.rs) and inject (context.rs scrub_and_trim + handover)
- [x] DEAD-CODE — object_field has 3 call sites; no other new symbol
- [x] SEMANTIC — hooks.md table now lists all 11 kinds matching BUILTIN_RAW order/names; stale gh[pos]/JWT doc patterns corrected to the real ones while there

### GATE RECORD
Outcome: PASS
Reviewed by: Claude (auto-gate, evidence above; security-sensitive scope is redaction-additive only) · date: 2026-07-14

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch: redaction counts per kind (apply() return already counts); false-positive reports on KV_SECRET

### Spec delta

### Competency deltas
