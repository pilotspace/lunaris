# TASK: Rebrand hook capture source prefix codex:/claude-code: -> lunaris:

slug: hook-source-prefix-lunaris · created: 2026-07-14 · stage: production
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
- CAPTURE emit sites (produce the source label):
  - `crates/lunaris-hook/src/context.rs` — CaptureToolCall→`codex:tool_call:pre` (322), CaptureToolResult→`codex:tool_call:post` (327), turn feedback→`codex:turn_feedback` (860), memory injection→`codex:memory_injection` (886).
  - `crates/lunaris-hook/src/ingest.rs` — the Claude-Code hook adapter: `claude-code:pre_tool_use|post_tool_use|stop|session_start|session_end` (38/52/69/82/95 + dedupe paths 199/218/234/245/263).
  - `crates/lunaris-hook/src/embed_promotion.rs:353` — promotion event `source: "codex:tool_call:post"` (+ tests 278/286).
- MATCHER sites (branch on the label — MUST stay in lock-step or gating breaks):
  - `context.rs::excluded_context_source` (1195), `is_toolcall_capture` (1207), `injectable_at_phase` (via is_toolcall_capture), `source_priority` (1235), `is_low_value_text` (1274).
  - `crates/lunaris-core/src/snippet.rs:88` — `summarize_json` prompt-envelope branch keys on `claude-code:pre_tool_use || codex:tool_call:pre` (shared by MCP recall AND hook inject — miss this and curation breaks for the new prefix).
- DOC/TEST sites: `crates/lunaris-mcp/src/main.rs` source_prefix examples (143/359), `crates/lunaris/tests/digest_recent_by_source.rs:167`, `docs/integration/{hooks,codex}.md`.
Context (working folder): source prefix is a recall-filter key (`filters.source_prefix`) — a public-ish contract; existing episodes keep their old prefix (semantic recall still finds them; only prefix-filter changes).
Honors (patterns / conventions): `#![forbid(unsafe_code)]` in lunaris-hook (tests stay env-free/pure); shared curation helper lives in lunaris-core::snippet (RC-1). Do NOT rename the socket/daemon (`codex-contextd.sock`) — that's wired in settings.json; only the source-string labels change.
Anchors the contract cites: the 9 source literals + `is_toolcall_capture`, `injectable_at_phase`, `excluded_context_source`, `source_priority`, `is_low_value_text`, `snippet::summarize_json`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Rebrand every Lunaris-hook capture source prefix to a single `lunaris:` namespace (was `codex:*` + `claude-code:*`).
Framings weighed: unify both agent prefixes under `lunaris:` (chosen, user-approved) · rename only `codex:*` keeping `claude-code:*` (rejected: user wants one product namespace)
Must:
<must>
  - All new captures emit a `lunaris:` source prefix (suffixes preserved): `lunaris:tool_call:pre|post`, `lunaris:turn_feedback`, `lunaris:memory_injection`, `lunaris:pre_tool_use|post_tool_use|stop|session_start|session_end`.
  - Every matcher that branched on the old prefixes recognizes the `lunaris:` equivalents IN LOCK-STEP: `is_toolcall_capture`, `injectable_at_phase`, `excluded_context_source`, `source_priority` (context.rs) and `snippet::summarize_json` prompt-envelope branch (lunaris-core). No matcher may keep keying only the retired prefix.
  - Observable behavior is UNCHANGED after the rename: tool-call captures still excluded from prompt-phase injection; prompt envelopes still curate to `prompt:`; source priority ordering preserved (post > pre; decision/edit still outrank).
  - The socket/daemon name (`codex-contextd.sock`) is NOT renamed — only source-string labels.
</must>
Reject:
<reject>
  - A capture emitted with a `lunaris:` prefix but a matcher still keyed to the old prefix -> silent gating breakage (the failure mode this task exists to prevent; caught by the discriminating tests).
</reject>
After:
<after>
  - `grep -rn '"codex:' crates/lunaris-hook crates/lunaris-core` returns zero non-comment source literals; same for `"claude-code:`.
  - Hook + core + mcp + lunaris test suites green with the new prefixes.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ The suffix asymmetry is acceptable — the codex path used `tool_call:pre/post` while the claude-code path used `pre_tool_use/post_tool_use`; unifying only the PREFIX leaves both suffix styles under `lunaris:` (per the approved preview). If wrong (user wanted suffixes normalized too): a follow-up to canonicalize suffixes. Confirmed by the AskUserQuestion preview which kept the suffixes.
  - [x] Old episodes keep their old prefix — accepted: semantic recall still finds them; only `source_prefix` filtering changes. No data migration in scope.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: tool-call capture recognized under the new prefix
  Given a hit with source "lunaris:tool_call:post" (and "lunaris:post_tool_use")
  When is_toolcall_capture is asked
  Then it returns true
  And injectable_at_phase("prompt", it, include_toolcalls=false) returns false (still excluded from prompt phase)

Scenario: prompt envelope still curates under the new prefix
  Given a pre-tool envelope with source "lunaris:tool_call:pre" carrying only a path (no new_string)
  When snippet::summarize_json runs
  Then it returns None (the pre-tool path-only drop), same as the old prefix did
  And a UserPromptSubmit envelope still renders as "prompt: …"

Scenario: capture sites emit the lunaris prefix
  Given the hook captures a tool result / a session_start
  When the episode source is read
  Then it is "lunaris:tool_call:post" / "lunaris:session_start" (never codex:/claude-code:)
  And no old-prefix literal remains in lunaris-hook/lunaris-core source

Scenario: source priority order preserved
  Given sources lunaris:tool_call:post and lunaris:tool_call:pre
  When source_priority ranks them
  Then post (75) outranks pre (55), and decision:/edit: still outrank both
  And the numeric ordering matches the pre-rename behavior
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
# Source-prefix rename (string contract — the recall filters.source_prefix key)
codex:tool_call:pre        -> lunaris:tool_call:pre
codex:tool_call:post       -> lunaris:tool_call:post
codex:turn_feedback        -> lunaris:turn_feedback
codex:memory_injection     -> lunaris:memory_injection
claude-code:pre_tool_use   -> lunaris:pre_tool_use
claude-code:post_tool_use  -> lunaris:post_tool_use
claude-code:stop           -> lunaris:stop
claude-code:session_start  -> lunaris:session_start
claude-code:session_end    -> lunaris:session_end

# Matchers updated in lock-step (recognize lunaris:* equivalents):
context.rs: is_toolcall_capture, injectable_at_phase, excluded_context_source, source_priority, is_low_value_text
lunaris-core/snippet.rs: summarize_json prompt-envelope branch
# NOT renamed: codex-contextd.sock (daemon/socket), docs/integration/codex.md filename.
Access pattern: string labels only — no storage schema change; old episodes retain their prefix.
```

Status: FROZEN @ v1 — approved by Tin Dang (2026-07-14, "use lunaris prefix instead" + AskUserQuestion "unify both").
Least-sure flag surfaced at freeze: [spec] the suffix asymmetry survives the rename (`lunaris:tool_call:post` from the codex path AND `lunaris:post_tool_use` from the claude-code path both exist) — matches the approved preview; if the user wanted a single canonical suffix too, that's a follow-up. Changing this mapping = change request back to SPECIFY.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: matcher lock-step 100% (the failure mode is a missed matcher).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_lunaris_toolcall_recognized (context.rs): is_toolcall_capture("lunaris:tool_call:post")==true, ("lunaris:post_tool_use")==true; injectable_at_phase("prompt", both, false)==false; injectable_at_phase("post_tool", …)==true. (scenario: tool-call capture recognized)
  - test_lunaris_prompt_envelope_curates (lunaris-core snippet.rs): summarize_json("lunaris:tool_call:pre", {path-only}) == None; a prompt payload renders "prompt: …". (scenario: prompt envelope curates)
  - test_lunaris_source_priority (context.rs): source_priority("lunaris:tool_call:post")=75 > ("lunaris:tool_call:pre")=55; decision:/edit: unchanged. (scenario: priority order preserved)
  - assertion (After): no `"codex:` / `"claude-code:` non-comment literals remain in lunaris-hook + lunaris-core src (grep gate in verify). (scenario: capture sites emit lunaris prefix)
  - existing hook/core tests updated to the new prefixes stay green (regression: observable behavior unchanged).
</test_plan>

Tests live in: `crates/lunaris-hook/src/context.rs` · `crates/lunaris-core/src/snippet.rs` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-hook/src/context.rs` `crates/lunaris-hook/src/ingest.rs` `crates/lunaris-hook/src/embed_promotion.rs` `crates/lunaris-core/src/snippet.rs` `crates/lunaris-mcp/src/main.rs` `crates/lunaris/tests/digest_recent_by_source.rs` `docs/integration/hooks.md` `docs/integration/codex.md`
Strategy (ordered batches):
  1. Matchers first (so tests can go red→green): context.rs is_toolcall_capture/excluded_context_source/source_priority/is_low_value_text + snippet.rs summarize_json recognize lunaris:* .
  2. Capture emit sites: context.rs (322/327/860/886), ingest.rs (all claude-code:*), embed_promotion.rs (353).
  3. Tests in those files updated to lunaris:* ; MCP doc-string examples + docs prefix references.
Safety rule (feature-specific): matcher recognition and capture labels must land in the SAME change — never a capture emitting lunaris:* while a matcher still keys the old prefix (silent gating breakage).
Code lives in: the crates listed above.
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — lunaris-hook lib 34/34 + integration 19/2/10; lunaris-core lib 81/81 (incl. snippet 5/5); lunaris digest integration 5/5; clippy `-D warnings --all-targets` on lunaris-hook/-core/-mcp = exit 0
- [x] coverage did not decrease — 3 new tests added (context.rs lunaris_toolcall_prefix_recognized / lunaris_source_priority_order_preserved / lunaris_excluded_sources_recognized; snippet.rs lunaris_prompt_prefix_curates); all pre-existing tests retained, only their literal prefix strings updated
- [x] no test or contract was altered during build — §3 CONTRACT frozen @ v1, untouched; tests updated only where the frozen contract renamed the literal (the rename IS the contract)
- [x] the green was EARNED, not gamed — matchers (excluded_context_source/is_toolcall_capture/source_priority/is_low_value_text/injectable_at_phase) AND emit sites BOTH key on `lunaris:` (grep-confirmed: context.rs emit 322/329/866/892 ↔ match 1201-1247/1280/1730-1734; ingest.rs 38-255; embed_promotion.rs 353). No split-brain gating, no stubbed logic — pure literal rename with symmetric coverage.
- [x] concurrency / timing of the risky operation is safe — no concurrency surface touched; string-literal rename only, no lock/await/spawn changes
- [x] no exposed secrets, injection openings, or unexpected dependencies — no new deps; `grep -rnE '"(codex|claude-code):'` over crates/*.rs returns NONE (5 remaining `codex_payload`/`codex_hook_event_name` are Codex-wire envelope FIELD names, correctly untouched)
- [x] layering & dependencies follow CONVENTIONS.md — shared curation stays in `lunaris-core::snippet` (RC-1 cross-crate precedent); no new local helpers
- [x] a person reviewed and approved the change — auto-gate under `autonomy: auto` (mechanical rename, non-security); owner reviews at PR #55 merge

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
> Pre-declare the OBSERVABLE outcomes a correct build must produce — derived from §2 SCENARIOS
> + §3 CONTRACT — so this gate checks the build is RIGHT, not merely that tests are green. Each
> row is evidence you can SEE, not a restatement of a test name.
- [x] A tool-call capture emitted by the contextd path carries source `lunaris:tool_call:post` (not `codex:tool_call:post`) — confirmed by context.rs:329 emit site + is_toolcall_capture recognizing it (test lunaris_toolcall_prefix_recognized)
- [x] A Claude Code hook capture (ingest.rs adapter) carries source `lunaris:pre_tool_use`/`lunaris:post_tool_use`/`lunaris:stop`/`lunaris:session_start`/`lunaris:session_end` (not `claude-code:*`) — confirmed by ingest.rs:38-255
- [x] Recall/inject curation summarizes a `lunaris:pre_tool_use` prompt envelope and drops path-only pre-tool envelopes — confirmed by snippet.rs:88 + test lunaris_prompt_prefix_curates
- [x] source_priority ranking preserved under the new prefix (tool_call:post=75 > pre=55 > post_tool_use=70 > pre_tool_use=45; decision/edit still outrank) — confirmed by test lunaris_source_priority_order_preserved

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — no new symbols introduced (rename only); every renamed literal has both a live emit site and a live matcher (grep-confirmed symmetric)
- [x] DEAD-CODE (code) — no orphaned symbol; the 3 new tests exercise the renamed matchers; zero old-prefix literals remain (grep NONE)
- [x] SEMANTIC (prose / non-code) — read docs/integration/hooks.md + codex.md prefix references + lunaris-mcp/src/main.rs doc-string examples (lines 143/359); all bare `codex:`/`claude-code:` source-prefix references renamed to `lunaris:*`; Codex envelope field names (codex_payload/codex_hook_event_name) intentionally retained as wire contract

### GATE RECORD
Outcome: PASS
If RISK-ACCEPTED -> owner: <name> · ticket: <link> · expires: <date>   (never for a security gap)
Reviewed by: auto-gate (autonomy: auto — mechanical rename, non-security) · date: 2026-07-14

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
