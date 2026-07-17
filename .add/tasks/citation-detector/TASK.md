# TASK: Citation Detector

slug: citation-detector · created: 2026-07-17 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it, or run `add.py autonomy set`. -->
phase: tests   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower the
     autonomy level to `manual` or `conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-hook/src/context.rs:993` — `capture_feedback(scope,
  session_id, injected_memory_ids, outcome)`: writes `lunaris:turn_feedback`
  via `capture_lightweight`; today emits ZERO activation signal and its
  `injected_memory_ids` input is ALWAYS `[]` in production (adapter reads a
  key the Stop payload never carries). The detector lands here (contextd
  socket path — the only path with an engine handle).
- `crates/lunaris-hook/src/context.rs:1040-1063` — `trace_injection`'s
  `handle_for_scope(scope) → handle.scoped().record_activation_refs()`
  log-and-continue pattern: the EXACT wiring the strong-ref upgrade mirrors.
- `crates/lunaris-hook/src/context.rs:420-429` — `ContextRequest::
  TurnFeedback` dispatch; wire variant at :99-105. `transcript_path` is NOT
  forwarded by the adapter today (dropped in run_feedback).
- `scripts/lunaris-codex-hook-adapter.py:325-335` — `run_feedback` builds
  the turn_feedback socket request (single sender for BOTH Claude Code and
  Codex; `--target` flag). Must start forwarding `transcript_path`.
- `crates/lunaris-core/src/activation.rs` — `RefSignal{id,grain,strength}`,
  `Grain::{Turn,ToolCall}`, `Strength::{Weak,Strong}` (task 2, this branch).
- TRANSCRIPT GROUND TRUTH (verified empirically on THIS machine,
  2026-07-17, 3 real transcripts under ~/.claude/projects/...):
  - JSONL entries: `type ∈ {assistant, user, attachment, system, ...}`.
  - Injections appear VERBATIM as `attachment.type ==
    "hook_additional_context"` with keys {content, hookEvent, hookName,
    toolUseID, type}; `content` holds the `<lunaris_memory_context
    phase=".." ...>` block; each memory line is
    `- [source=<s> score=<f> id=<26-char ULID>] <snippet>`.
  - Assistant tool calls: `type=="assistant"`,
    `.message.content[].type=="tool_use"` (has `id`, `name`).
  - Tool outcomes: `type=="user"`, `.message.content[].type=="tool_result"`
    with `tool_use_id` and **`is_error: true|false|null`** (856 false / 41
    true / 577 null in a 10k-line sample) — the structured success signal;
    `tool_response.success` / `.exit_code` exist only as ad-hoc blob keys.
  - The final assistant message = last `type=="assistant"` entry's text
    blocks.
- `crates/lunaris-hook/src/context.rs:1564-1571, 2213-2266` — test harness:
  `service_with_seeded_scope` + `insert_handle_for_test` +
  `insert_storage_for_test` (both caches MUST seed the same backing store);
  `trace_injection_emits_weak_activation_refs` is the scenario-mirror.
Context (working folder): `.add/milestones/engram-soul-loop/MILESTONE.md`
  task 3 + the 2026-07-17 tool-call-grain amendment; Explore dossier
  (session artifact) — key findings inlined above.
Honors (patterns / conventions): fail-open on the turn path (warn, never
  error); no transcript fixture exists in-repo (net-new fixture required);
  ids in the injection block are CHUNK/FACT ulids (Hit.id), same namespace
  task 2's ledger already keys by — consistent end-to-end, no episode-id
  translation.
Anchors the contract cites: `transcript::TurnTranscript` (new) ·
  `citation::grade_injections` (new) · `capture_feedback` ·
  `record_activation_refs` · `RefSignal` · `run_feedback` (adapter).

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: mechanical Stop-time citation detector — transcript-derived
  per-memory verdicts upgrading weak injections to strong activation refs
Framings weighed: parse the turn's transcript JSONL at Stop (chosen —
  verified: injections, final message, tool outcomes all live there; no
  new injection-time state) · persist injected snippets at inject time in
  a per-session pad (rejected: new state file + lifecycle for data the
  transcript already holds) · storage-scan lunaris:memory_injection by
  session_id at Stop (rejected: has ids but NOT snippet text, and adds a
  storage scan per Stop).
Must:
<must>
  - the adapter's feedback mode forwards `transcript_path` (and drops the
    dead `injected_memory_ids` read) into the turn_feedback request; the
    wire variant gains `transcript_path: Option<String>` (additive)
  - at Stop, the detector parses the transcript JSONL and recovers, for
    THIS session's turns since the last Stop (bounded: tail window, see
    contract): (a) injected memories — every `hook_additional_context`
    attachment whose content contains `<lunaris_memory_context`, parsed
    per-line into (memory_id, snippet); (b) the final assistant message
    text; (c) tool calls — assistant `tool_use` (id, name) joined to user
    `tool_result` (tool_use_id, is_error)
  - citation = mechanical n-gram overlap, NO LLM: a memory is `cited`
    when its snippet shares ≥1 distinctive n-gram (len ≥ N_GRAM tokens,
    stopword-filtered) with the final assistant message; else `uncited`
  - tool-call grain (2026-07-17 amendment): a memory injected at
    `phase="post_tool"` is attributed to the tool_use whose attachment
    `toolUseID` matches; if that tool call's is_error==false →
    strong/ToolCall signal; is_error==true → stays weak (never negative
    here — judging is the dream pass's job)
  - verdict → activation: cited → RefSignal{Strong, Turn};
    tool-call-attributed success → RefSignal{Strong, ToolCall}; injected
    but uncited/unattributed → NO new signal (the weak ref already landed
    at inject time; Stop must not double-count weak)
  - capture_feedback writes per-memory verdicts into the turn_feedback
    meta: `verdicts: [{id, verdict: cited|uncited, grain, tool_use_id?}]`
    (additive next to the legacy flat id array), and emits the strong
    RefSignals via handle_for_scope → record_activation_refs with the
    trace_injection log-and-continue pattern
  - everything is fail-open on the turn path: missing/unreadable/foreign
    transcript, zero injections found, parse errors → turn_feedback still
    written (with empty verdicts + a `detector: skipped_<reason>` meta),
    exit 0
  - the transcript read is BOUNDED: read at most the trailing
    TRANSCRIPT_TAIL_BYTES (default 4 MiB, env-overridable) of the file —
    a week-long session must not make Stop O(file)
</must>
Reject:
<reject>
  - transcript_path absent/unreadable -> verdicts empty +
    detector:"skipped_no_transcript"; capture written; no error
  - transcript JSONL line malformed -> skip the line (lenient per-line
    parse); never abort the pass
  - injection block with unparseable memory line -> skip that line, keep
    the rest of the block
  - session_id mismatch (transcript belongs to another session) ->
    verdicts empty + detector:"skipped_session_mismatch" (guards against
    resumed-session path reuse)
  - activation write failure -> tracing::warn, feedback capture still
    succeeds (same contract as trace_injection)
</reject>
After:
<after>
  - a memory whose snippet text demonstrably shaped the final answer
    carries a Strong/Turn ledger ref after the turn ends; its activation
    outranks an injected-but-ignored peer on the next recall (milestone
    exit criterion: "cited twice outranks equal-similarity uncited")
  - turn_feedback records become per-memory ground truth the dream pass
    (task 8) and telemetry (task 10) can consume
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ [spec] n-gram overlap (N_GRAM=5 tokens, stopword-filtered, distinctive
    = appears in exactly one injected snippet) separates cited from
    uncited without an LLM — lowest confidence because snippets are ≤260
    chars and answers paraphrase; if wrong: verdicts skew uncited (missed
    reinforcement — degrades toward today's baseline, never corrupts) and
    N_GRAM/matching retunes in the dream-pass judge instead.
  ⚠ [spec] `hook_additional_context` attachments carry the SAME
    session_id stream as the Stop payload's transcript_path — verified on
    3 transcripts here, but resumed sessions (`claude --resume`) reuse a
    file across session_ids; if wrong: the session-mismatch reject fires
    and the detector skips (safe, observable via detector: meta).
  - [x] is_error==null means "no structured outcome" (577/10k sample) —
    treated as NOT successful for strong-grading purposes (conservative).
  - [x] ids in injection blocks are the same chunk/fact ulid namespace the
    ledger keys by — task 2 established this end-to-end.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: cited memory upgrades to a strong turn ref
  Given a transcript fixture where memory M1's snippet contains the
    distinctive phrase "granite embedder resolves after llamacpp" and the
    final assistant message repeats that phrase, while M2's snippet does not
  When the Stop feedback pass runs with the fixture's transcript_path
  Then the turn_feedback meta carries verdicts
    [{M1, cited, turn}, {M2, uncited, turn}]
  And M1's activation record gains ONE Strong/Turn ref
  And M2's record gains NOTHING at Stop (weak landed at inject time)

Scenario: post_tool injection graded by its tool call's outcome
  Given a fixture with two post_tool injections — M3 attached to a
    tool_use whose tool_result has is_error=false, M4 attached to one
    with is_error=true
  When the pass runs
  Then M3 gains a Strong/ToolCall ref and M4 gains no strong ref
  And M4's verdict row records its tool_use_id (dream-pass evidence)

Scenario: adapter forwards transcript_path
  Given a Stop event JSON with transcript_path set
  When run_feedback builds the socket request
  Then the request contains transcript_path verbatim
  And no injected_memory_ids key is read off the raw event anymore

Scenario: no transcript → fail-open
  Given a TurnFeedback request with transcript_path=None (or unreadable path)
  When the pass runs
  Then turn_feedback is captured with empty verdicts and
    detector="skipped_no_transcript"
  And the call returns Ok (exit 0 for the hook)

Scenario: malformed lines never abort
  Given a transcript containing a garbage line, a valid injection, and a
    valid final message
  When the pass runs
  Then the valid injection is still graded
  And no error surfaces

Scenario: session mismatch skips detection
  Given a transcript whose session ids differ from the request's session_id
  When the pass runs
  Then verdicts are empty with detector="skipped_session_mismatch"
  And no activation refs are written

Scenario: tail window bounds the read
  Given a transcript file larger than the tail budget where the only
    injection sits inside the final tail window
  When the pass runs
  Then the injection is found (tail covers it) and the bytes read never
    exceed TRANSCRIPT_TAIL_BYTES (counting reader wrapper in the test)

Scenario: activation write failure stays fail-open
  Given a storage whose atomic_write fails for activation keys
  When the pass runs on a fixture with a cited memory
  Then the turn_feedback capture is still written and Ok is returned
  And a warn is traced
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
// lunaris-hook (new module crates/lunaris-hook/src/transcript.rs)
pub struct InjectedMemory { pub id: Ulid, pub snippet: String,
                            pub phase: String, pub tool_use_id: Option<String> }
pub struct ToolOutcome    { pub tool_use_id: String, pub is_error: Option<bool> }
pub struct TurnTranscript { pub injections: Vec<InjectedMemory>,
                            pub tool_outcomes: Vec<ToolOutcome>,
                            pub final_assistant_text: String,
                            pub session_ids_seen: HashSet<String> }
pub fn read_turn_transcript(path: &Path, tail_bytes: u64)
    -> std::io::Result<TurnTranscript>
  // tail-bounded read (default TRANSCRIPT_TAIL_BYTES=4MiB via
  // LUNARIS_TRANSCRIPT_TAIL_BYTES); lenient per-line serde; first partial
  // line after a mid-file seek is discarded

// lunaris-hook (new module crates/lunaris-hook/src/citation.rs)
#[serde(rename_all="snake_case")] pub enum Verdict { Cited, Uncited }
pub struct MemoryVerdict { pub id: Ulid, pub verdict: Verdict,
                           pub grain: Grain, pub tool_use_id: Option<String> }
pub fn grade_injections(t: &TurnTranscript) -> Vec<MemoryVerdict>
  // pure fn, NO IO/LLM. cited := snippet shares >=1 distinctive n-gram
  // (N_GRAM=5 lowercase alnum tokens, stopword-filtered; distinctive =
  // that n-gram occurs in exactly one injected snippet) with
  // final_assistant_text. post_tool injections with a matching
  // tool_use_id whose is_error==Some(false) -> grain=ToolCall (strong);
  // is_error Some(true)|None -> stays Uncited/Turn unless text-cited.
  // Per-memory dedupe: same id injected twice grades once (best verdict).

// wire + handler (context.rs)
ContextRequest::TurnFeedback { ..., transcript_path: Option<String> }  // additive
capture_feedback(..., transcript_path: Option<String>):
  verdicts = transcript-derived (fail-open: skipped_<reason> meta on any miss)
  meta += { "verdicts": [ {id, verdict, grain, tool_use_id?} ... ],
            "detector": "ok" | "skipped_no_transcript" |
                        "skipped_session_mismatch" }
  strong signals: Cited -> RefSignal{Strong,Turn};
                  ToolCall-graded -> RefSignal{Strong,ToolCall};
  emitted via handle_for_scope(scope) log-and-continue (trace_injection
  pattern); Uncited emits NOTHING (weak already landed at inject).

// adapter (scripts/lunaris-codex-hook-adapter.py run_feedback)
request += { "transcript_path": event.get("transcript_path") }
request -= injected_memory_ids raw-event read (dead; server derives)
  // wire key stays OPTIONAL for old-adapter compat (serde default)

Schema: no storage shape change — verdicts ride turn_feedback episode
  metadata; activation writes reuse task 2's records/key. Fixture:
  crates/lunaris-hook/tests/fixtures/transcript_citation.jsonl (net-new,
  distilled from the verified real schema: hook_additional_context
  attachments + assistant/user entries + is_error variants).
```

Status: FROZEN @ v1 — approved by autonomous (autonomy=auto; design locked
in MILESTONE.md task 3 + the 2026-07-17 tool-call-grain amendment; ground
truth verified on real transcripts this session).
Least-sure flag surfaced at freeze: [spec] the mechanical n-gram rule
(N_GRAM=5, distinctive-in-one-snippet) is the whole cited/uncited boundary
— why: snippets are <=260 chars and assistant answers paraphrase, so
recall of the rule is unproven at corpus scale; cost if wrong: verdicts
skew Uncited → missed strong refs, ranking degrades toward today's
similarity-only baseline (never corruption; dream-pass LLM judge is the
designed backstop). Second flag: [contract] `is_error==None` counted as
not-successful — conservative by design; if evidence shows None-heavy
tools (577/1474 in sample) starve ToolCall credit, loosen in task 8's
judge, not here.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every §2 scenario; red = missing-symbol compile failure
for the new modules + assertion-red where surfaces exist (adapter test).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - crates/lunaris-hook/tests/fixtures/transcript_citation.jsonl (net-new
    fixture distilled from the VERIFIED real schema; includes: 2 prompt
    injections M1/M2, 2 post_tool injections M3(is_error=false)/
    M4(is_error=true), garbage line, final assistant message citing M1's
    distinctive phrase, consistent session ids) + a variant fixture with
    mismatched session ids
  - crates/lunaris-hook/src/transcript.rs unit tests (in-module):
    - reads_injections_tools_and_final_text (fixture round-trip)
    - malformed_lines_are_skipped
    - tail_window_bounds_the_read (big synthetic file; injection in tail;
      byte-count wrapper)
  - crates/lunaris-hook/src/citation.rs unit tests (pure fn):
    - cited_vs_uncited_by_distinctive_ngram (M1 cited, M2 uncited)
    - post_tool_success_upgrades_to_toolcall_grain (M3 strong, M4 not)
    - duplicate_injection_grades_once
  - crates/lunaris-hook/src/context.rs in-module tests (harness:
    service_with_seeded_scope + insert_storage_for_test):
    - feedback_pass_writes_verdicts_and_strong_refs (end-to-end on the
      fixture: verdicts meta + M1 Strong/Turn + M3 Strong/ToolCall in the
      ledger; M2/M4 no strong ref)
    - feedback_pass_fail_open_no_transcript (detector=skipped_no_transcript,
      capture written, Ok)
    - feedback_pass_session_mismatch_skips (no refs written)
    - feedback_pass_activation_write_failure_still_ok (failing storage)
  - scripts/tests/test_adapter_feedback_transcript.py (existing python
    test dir pattern): run_feedback forwards transcript_path verbatim and
    no longer reads injected_memory_ids off the raw event (assertion-red
    against current adapter)
</test_plan>

Tests live in: `./tests/` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `./src/`   <fill before the §3 freeze — every file the build may write>
Strategy (ordered batches): <1. … 2. … — the planned build order; guidance, not enforced>
Safety rule (feature-specific): <e.g. debit+credit in one atomic transaction>
Code lives in: `./src/`
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
- [ ] <observable outcome a correct build must produce> — confirmed by <how / where>
- [ ] <another observable outcome> — confirmed by <evidence seen>

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [ ] WIRING (code) — every new symbol is referenced; record where / how confirmed
- [ ] DEAD-CODE (code) — no new unused or orphaned symbol introduced
- [ ] SEMANTIC (prose / non-code) — read in full, not skimmed: <what read · what confirmed>

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
If RISK-ACCEPTED -> owner: <name> · ticket: <link> · expires: <date>   (never for a security gap)
Reviewed by: <name> · date: <date>

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
