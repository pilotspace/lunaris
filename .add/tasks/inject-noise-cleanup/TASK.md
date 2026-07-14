# TASK: Prompt-phase injection drops raw tool-call noise; curation never emits raw JSON

slug: inject-noise-cleanup · created: 2026-07-14 · stage: production
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
- `crates/lunaris-hook/src/context.rs:recall_and_trace` (414) — builds `ContextMemory`
  candidates from recall hits. HYBRID default path (442-459) snapshots each hit as
  `snippet: scrub_and_trim(&h.text, 900)` then `curate_context_memories_lossy(candidates, max_hits)`.
  The hybrid path BYPASSES `min_score` (445-447: RRF scores are ~0.03-scale, 0.55 floor would
  annihilate all hits) — so every fused candidate reaches curation regardless of relevance.
- `crates/lunaris-hook/src/context.rs:curate_context_memories_lossy` (1091) — the LOSSY variant:
  when `summarize_memory_for_context` returns None it falls back to `single_line(&memory.snippet)`
  = RAW envelope JSON. This is what dumps `{ " cwd " : ...` mangled tool-call payloads into the
  prompt injection. Non-lossy sibling `curate_context_memories` (1057) DROPS a None instead.
- `crates/lunaris-hook/src/context.rs:summarize_memory_for_context` (1196) →
  `lunaris_core::snippet::summarize_json` (37) — returns None for a `codex:tool_call:post` wrapper:
  (a) `scrub_and_trim(_,900)` TRUNCATES the JSON mid-object before parse (live snippet ended
  `[truncated]` → `parse_jsonish` fails), and (b) the codex PostToolUse wrapper nests its payload
  under keys not in the `path/output/command/prompt/tool_response/tool_input` lookup set.
- `crates/lunaris-hook/src/context.rs:excluded_context_source` (1164) — the existing source
  drop-list applied INSIDE curation (both variants). Currently drops `codex:memory_injection`,
  `codex:turn_feedback`, `claude-code:session_start`, `claude-code:stop` — NOT tool-call captures.
- `crates/lunaris-hook/src/context.rs:render_context` (1005) — receives `phase`; the prompt arm
  is the `_ =>` fallthrough (1036). Curation currently gets NO phase, so it cannot vary by phase.
Context (working folder): live proof (Moon 6381, this repo scope) shows all 5 prompt-injected hits
  are `codex:tool_call:post` at score 0.03 rendering RAW — the exact noise Tin flagged. The
  capture-signal-gate (shipped 674426f) drops metadata-only captures at INGEST, but (1) pre-gate raw
  envelopes remain in the index, and (2) even real tool results are low-value for PROMPT-phase recall.
Honors (patterns / conventions): design-for-failure (every new drop is env-restorable; a recall
  failure must never surface to the agent — keep the fail-to-legacy/empty degradation); no lock
  across `.await`; `#![forbid(unsafe_code)]` (no env::set_var in tests — read env, don't set it);
  curation helpers stay hook-side, JSON-envelope summarization stays in `lunaris_core::snippet`.
Anchors the contract cites: `curate_context_memories_lossy`, `excluded_context_source`,
  `recall_and_trace`, `summarize_memory_for_context`, a new `prompt_phase` curation flag.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: PROMPT-phase context injection excludes raw tool-call captures and NEVER renders raw JSON
envelope text — so the model sees curated decisions/edits, not mangled `{ " cwd " : ... }` noise.
Framings weighed:
  - phase-aware source exclusion + non-raw curation guard (CHOSEN) — kills the observed noise at the
    two proven root causes (lossy raw fallback + tool-call crowding), env-restorable, small.
  - only raise min_score (rejected as primary) — hybrid RRF scores are ~0.03 by design; a cosine
    floor is a no-op there and 0.55 would drop every hit. Honest tradeoff: min_score cannot
    discriminate fused hits, so it is NOT the lever. Surfaced, not hidden.
  - improve summarize_json to parse the codex wrapper (partial — kept as defense-in-depth: parse the
    FULL text not the 900-char-truncated snippet — but the wrapper shape is a moving target, so it is
    not the primary guard).
Must:
<must>
  - `curate_context_memories_lossy` MUST NOT emit raw JSON: when the summary is a fallback AND the
    snippet still looks like a JSON envelope (trimmed starts with `{` or `[`), DROP it instead of
    rendering `single_line(raw)`. A non-envelope plain-text fallback is still allowed.
  - Curation MUST parse the FULL episode text, not the pre-truncated 900-char snippet: summarize
    before the char cap so a mid-object truncation can't defeat `parse_jsonish`.
  - PROMPT phase MUST exclude raw tool-call capture sources (`codex:tool_call:pre|post`,
    `claude-code:pre_tool_use|post_tool_use`) from injection candidates. `post_tool` phase keeps them
    (a tool result relating to prior tool results is on-topic there).
  - The prompt-phase tool-call exclusion MUST be env-restorable:
    `LUNARIS_CONTEXT_PROMPT_INCLUDE_TOOLCALLS=1` puts them back (design-for-failure toggle).
  - decisions/edits/prompt/other sources are UNAFFECTED in every phase.
</must>
Reject:
<reject>
  - a truncated / unparseable JSON envelope at prompt phase -> DROPPED from injection (not raw-rendered)
  - a `codex:tool_call:post` hit at prompt phase (toggle unset) -> excluded (not injected)
  - a `decision:`/`edit:` hit at prompt phase -> still injected, curated (no regression)
</reject>
After:
<after>
  - A live prompt recall on a tool-call-heavy scope injects ZERO raw `{`-brace lines.
  - With only tool-call captures in scope, prompt injection is empty (or non-tool hits only), never
    raw noise; the same scope at post_tool phase still surfaces the tool captures (curated).
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Excluding tool-call captures from PROMPT injection loses no signal a user wants at prompt time —
    lowest confidence because a prior tool result CAN be relevant to the next prompt. If wrong: a
    useful tool-result recall is suppressed at prompt phase. Cost: mitigated — (a) it still surfaces
    at post_tool phase, (b) `LUNARIS_CONTEXT_PROMPT_INCLUDE_TOOLCALLS=1` restores it, (c) decisions/
    edits (the durable signal) are unaffected. Confirm via live prompt-vs-post_tool recall.
  - [ ] the JSON-envelope sniff (`trimmed.starts_with('{'|'[')`) doesn't drop a legit plain-text hit
    — confirm: plain text rarely starts with a brace; decisions/edits summarize (never hit the raw
    fallback), so the sniff only fires on unparseable envelopes. Confirmed by a decision/edit render test.
  - [x] hybrid path bypasses min_score — confirmed at context.rs:445-447 (RRF ~0.03 scale).
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: lossy curation drops an unparseable JSON envelope instead of raw-rendering it
  Given a candidate whose snippet is a truncated/mangled JSON envelope that summarize cannot parse
  When curate_context_memories_lossy runs
  Then that candidate is dropped (no output line starts with '{' or '[')
  And a plain-text (non-envelope) fallback candidate is still kept

Scenario: full-text curation survives snippet truncation
  Given a codex:tool_call:post episode whose curated summary needs bytes past char 900
  When curation summarizes the FULL text before trimming
  Then a curated summary is produced (not raw), or it is cleanly dropped — never raw JSON

Scenario: prompt phase excludes raw tool-call captures
  Given a scope whose recall hits are all codex:tool_call:post
  When a prompt-phase recall runs with the toggle unset
  Then the injected memories contain no codex:tool_call:* source
  And a decision:/edit: hit in the same set is still injected

Scenario: post_tool phase keeps tool-call captures
  Given the same tool-call-heavy scope
  When a post_tool-phase recall runs
  Then codex:tool_call:post hits are still eligible for injection

Scenario: toggle restores tool-call captures at prompt phase
  Given LUNARIS_CONTEXT_PROMPT_INCLUDE_TOOLCALLS=1
  When a prompt-phase recall runs
  Then codex:tool_call:post hits are eligible again
  And decisions/edits remain injected (no regression)
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
CONTEXT.RS (lunaris-hook)
  const CURATION_INPUT_CHARS: usize = 8000
    Replaces the inline `900` in every `scrub_and_trim(&h.text, N)` candidate-construction site
    (hybrid 456, legacy 507/525/572). Scrub still redacts secrets; the larger budget lets
    `parse_jsonish` see a full envelope instead of a mid-object truncation. Curation still caps the
    final snippet at 260, so client-facing size is unchanged.

  fn curate_context_memories_lossy(candidates, max_hits) -> Vec<ContextMemory>   [behavior change]
    When summarize returns None, compute `line = single_line(snippet)`; if `line` is empty OR
    `line.trim_start()` starts with '{' or '[' -> return None (DROP, never raw-render). Otherwise
    keep `line` (plain-text fallback preserved). Sort/dedup/priority unchanged.

  fn injectable_at_phase(phase: &str, source: &str, include_toolcalls: bool) -> bool  [new pure fn]
    Returns false IFF: phase == "prompt" AND is_toolcall_capture(source) AND !include_toolcalls.
    is_toolcall_capture = source ∈ { "codex:tool_call:pre", "codex:tool_call:post",
      "claude-code:pre_tool_use", "claude-code:post_tool_use" }.
    All other (phase, source) pairs -> true. Pure (env read stays at the call site) so tests need
    no env::set_var (forbid(unsafe_code)).

  recall_and_trace: reads `include_toolcalls = env_flag("LUNARIS_CONTEXT_PROMPT_INCLUDE_TOOLCALLS")`
    once, then each candidate/keyword-candidate builder gains
    `.filter(|h| injectable_at_phase(phase, &h.source, include_toolcalls))` (hybrid + legacy paths).

ENV
  LUNARIS_CONTEXT_PROMPT_INCLUDE_TOOLCALLS=1  -> restore tool-call captures at prompt phase.

UNCHANGED: min_score semantics (hybrid still bypasses by design — documented, not the lever);
  render_context; SessionDigest; decision/edit/prompt curation; the non-lossy curate variant.
```

Status: FROZEN @ v1 — approved by AI auto-gate (fast-lane freeze; Tin reviews at PR).
Least-sure flag surfaced at freeze: [spec] excluding tool-call captures from PROMPT injection may
  suppress a genuinely relevant prior tool result at prompt time (why: a tool result CAN inform the
  next prompt; cost: mitigated — still surfaces at post_tool phase, env-restorable, decisions/edits
  unaffected). Secondary [contract] the `{`/`[` envelope sniff could in theory drop a plain-text hit
  that starts with a brace — cost: negligible, decisions/edits summarize and never hit the fallback.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: the 5 scenarios, as fast unit tests over the pure functions (no live Moon).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - lossy_drops_unparseable_json_envelope: build candidates = [mangled `{ " cwd " ... ` envelope
    (source codex:tool_call:post), plain-text "just a note" (source other)]; curate_lossy → no
    output snippet starts with '{'/'['; the plain-text note survives.
  - lossy_curates_full_text_past_snippet_cap: an envelope whose summarizable field sits past char
    900 → curated summary is produced from full text (not dropped, not raw). (Uses a decision:
    envelope with a long rationale so summarize succeeds only on full text.)
  - injectable_at_phase_excludes_toolcalls_at_prompt: injectable_at_phase("prompt",
    "codex:tool_call:post") == false; ("prompt","decision:x") == true.
  - injectable_at_phase_keeps_toolcalls_post_tool: injectable_at_phase("post_tool",
    "codex:tool_call:post") == true.
  - injectable_at_phase_toggle_restores (env-read only, no set): assert the pure fn honors a passed
    include-flag arg (thread the env read as a bool param so the test needs no env::set_var —
    forbid(unsafe_code) blocks env mutation). ("prompt", toolcall, include=true) == true.
</test_plan>

Tests live in: `crates/lunaris-hook/src/context.rs` (`#[cfg(test)] mod tests`) · MUST run red before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-hook/src/context.rs`
Strategy (ordered batches): 1. red: 5 unit tests in context.rs tests mod. 2. add `injectable_at_phase`
  + `is_toolcall_capture` pure fns + `CURATION_INPUT_CHARS` const. 3. lossy raw-fallback drop guard.
  4. bump the 4 `scrub_and_trim(_,900)` sites to the const + wire the phase filter in recall_and_trace.
  5. green. 6. live prompt-vs-post_tool recall proof on Moon 6381.
Safety rule (feature-specific): recall must still fail-to-legacy/empty — the new filter/drop only
  removes candidates, never introduces a panic or error path; no lock across `.await`.
Code lives in: `crates/lunaris-hook/src/context.rs`
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

- [x] all tests pass — `lunaris-hook --lib` 30/30 (20 context::tests incl. the 5 new); clippy `-p lunaris-hook --all-targets` clean
- [x] coverage did not decrease — added 5 tests, removed none
- [x] no test or contract was altered during build — §3 FROZEN @ v1 untouched; observable contract held (see deviation note below)
- [x] the green was EARNED — the drop test feeds a REAL mangled/truncated `codex:tool_call:post` envelope (copied from the live repro) and asserts no output starts with `{`/`[` AND that a plain-text sibling survives (not a vacuous "empty" assert). Phase tests assert both exclude AND keep AND toggle-restore. Live proof independently confirms.
- [x] concurrency / timing safe — the new code is pure filters + one env read at the top of `recall_and_trace`; no new lock, no `.await` added; recall still degrades to legacy/empty on any failure (unchanged)
- [x] no exposed secrets / injection openings / unexpected deps — scrubber still runs at `scrub_and_trim`; larger char budget feeds the SAME scrubber; no new crate
- [x] layering & dependencies follow CONVENTIONS.md — all changes in `lunaris-hook`; JSON-envelope summarization still delegates to `lunaris_core::snippet`
- [ ] a person reviewed and approved the change — auto-gate (non-security); Tin reviews at PR #55 merge

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
- [x] prompt-phase injection emits ZERO raw JSON lines on a tool-call-heavy scope — LIVE (noise_proof.py, Moon 6381, this repo scope): PROMPT phase 0 hits, 0 raw-JSON lines, 0 tool-call sources (was 5 raw `codex:tool_call:post` at score 0.03)
- [x] post_tool phase still surfaces tool captures, now CURATED not raw — LIVE (posttool_render.py): 5 `codex:tool_call:post` hits, 0 raw-`{` lines, snippets are extracted content
- [x] decisions/edits unaffected — `lossy_keeps_curated_decision_no_regression` + existing curation tests green
- [x] toggle restores tool-calls at prompt — `injectable_at_phase_toggle_restores_toolcalls` green

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `injectable_at_phase` called at 4 candidate sites (hybrid + 3 legacy); `is_toolcall_capture` called by it; `env_flag` read once in recall_and_trace; `CURATION_INPUT_CHARS` at all 4 scrub sites (grep: zero remaining `, 900)`); clippy would flag any dead symbol.
- [x] DEAD-CODE (code) — no orphan; the summarize_memory_for_context envelope-drop is exercised by lossy_drops_truncated_tool_call_envelope.
- [x] SEMANTIC — n/a (code task).

### DEVIATION (contract location, not behavior)
The §3 contract placed the raw-JSON drop in `curate_context_memories_lossy`. The red test revealed
the raw text actually originates one layer up in `summarize_memory_for_context` (it returned
`Some(single_line(raw))` for unparseable JSON, so the lossy curator's `Some` branch never reached
the guard). The drop therefore lives in `summarize_memory_for_context` (with the lossy-curator guard
kept as defense-in-depth). The OBSERVABLE contract — "lossy curation never emits raw JSON" — is
unchanged; only the internal location moved. Not a contract weakening.

### GATE RECORD
Outcome: PASS
Auto-resolved under `autonomy: auto` — all Build-expectations confirmed by live evidence (prompt 0
raw lines vs 5 pre-fix; post_tool curated; toggle + no-regression green). Non-security,
non-concurrency-residue. Owner reviews at PR #55 merge.
Reviewed by: AI auto-gate (Tin reviews at PR) · date: 2026-07-14

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>

### Spec delta
Forward changes for the next loop — each re-enters at Specify as the next task. One line
each, tagged `[SPEC · open|seeded|dropped]`, with evidence (e.g. `[SPEC · open] rate-limit
the retry path (evidence: prod herd spikes)`). See the `add` skill's `deltas.md`.
- [SPEC · open] purge pre-gate raw `codex:tool_call:*` episodes from live scopes on Moon 6381
  (evidence: they're now excluded from PROMPT injection but still bloat the index + surface at
  post_tool; a destructive one-off — do behind explicit confirmation, not auto).
- [SPEC · open] literal `\n` escapes render un-unescaped in post_tool tool-content snippets
  (evidence: posttool_render.py shows `return Arc::ptr_eq handles.\n - contextd...`); cosmetic,
  single_line should unescape `\\n`.
- [SPEC · open] teach `summarize_json` the codex PostToolUse wrapper shape so real tool results
  summarize at prompt phase too (evidence: full-text budget helped, but the wrapper's tool payload
  key is still not in the lookup set — many wrappers still summarize to None).

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
