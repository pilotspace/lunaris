# TASK: Context Savings Telemetry

slug: context-savings-telemetry · created: 2026-07-17 · stage: production
autonomy: auto
phase: tests   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> Milestone engram-soul-loop task 10 (added 2026-07-17, Tin "yes"): make the
> window-context saving MEASURABLE, not argued. Rides task 3's transcript
> reader (PR #69, merged). Recall-on/off A/B session comparison is the
> eval-harness follow-up, explicitly NOT in scope.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-hook/src/context.rs:trace_injection` (~1098) — writes the
  `lunaris:memory_injection` capture. Meta today: injection_id, phase,
  session_id?, memory_ids[]. Caller chain: `respond_with_memories` (~780)
  computes `rendered_context = render_context(...)` at line 788 THEN calls
  `spawn_trace_injection` (789) → spawned task calls `trace_injection`
  (1238). The rendered string's length is in scope at the spawn site —
  thread `rendered_context.len()` through `spawn_trace_injection` →
  `trace_injection` (both private, additive param is safe).
- `crates/lunaris-hook/src/context.rs:capture_feedback` (~1018) — writes
  `lunaris:turn_feedback` capture. Meta today: session_id?,
  injected_memory_ids[], detector, verdicts[]. `grade_turn_feedback`
  (~1290) returns `(Vec<MemoryVerdict>, &'static str)` and already parses
  the full `TurnTranscript` — transcript-derived stats are one struct away.
- `crates/lunaris-hook/src/transcript.rs:TurnTranscript` — carries
  `injections`, `tool_outcomes`, `final_assistant_text`; `read_turn_transcript`
  knows the file length (`file.metadata()?.len()`) but discards it today.
- `scripts/tests/test_adapter_feedback_transcript.py` — the stdlib-unittest
  pattern for python script tests (importlib load + mock).
- `scripts/test-recovery.py` — existing pattern for a python script that
  speaks RESP to Moon directly.

Context (working folder): `.add/milestones/engram-soul-loop/MILESTONE.md`
task 10 (a/b/c breakdown). Token heuristic: chars/4 (MILESTONE-mandated,
no tokenizer dependency).

Honors (patterns / conventions):
- Fail-open on the turn path — telemetry must never fail a capture.
- Additive meta only: existing meta keys keep their exact names/shapes
  (Memory Inspector + tests read them).
- env reads at call time, never cached in statics (issue #49 convention).
- Keyspace helpers from `lunaris_core::keyspace` only (RC-1).
- Scripts are stdlib-first python; tests runnable directly.

Anchors the contract cites: `trace_injection`, `spawn_trace_injection`,
`respond_with_memories`, `capture_feedback`, `grade_turn_feedback`,
`TurnTranscript`, `read_turn_transcript`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: context-savings telemetry (injection-side + Stop-side counters +
per-scope aggregation report)

Framings weighed: stamp-counters-into-existing-captures (chosen — zero new
storage kinds, rides existing meta) · new-telemetry-episode-kind (rejected:
new source needs exclusion-list + inspector churn) · metrics-endpoint-only
(rejected: loses per-turn provenance, can't compute cited-rate joins).

Must:
<must>
  - (a) `lunaris:memory_injection` meta gains `injected_chars` (usize =
    rendered_context.len()) and `injected_tokens_est` (= chars/4, integer
    division) — stamped from the REAL rendered injection string at the
    spawn site, not recomputed from memories.
  - (b) `lunaris:turn_feedback` meta gains `transcript_stats` object
    `{file_bytes, tool_call_count, final_text_chars}` whenever the
    detector ran (`detector == "ok"`); absent when skipped. Derived from
    the SAME transcript pass the citation grader uses — no second read.
  - (c) `scripts/context-savings-report.py` — per-scope aggregation over
    Moon: scans the scope's episode keys, filters
    `lunaris:memory_injection` / `lunaris:turn_feedback` sources, prints
    per-scope totals: injected tokens (sum of injected_tokens_est),
    turn count, tool-call count, cited/uncited verdict counts + cited
    rate. Pure aggregation core is a separate function taking parsed
    episode dicts (testable without Moon).
  - Rust counters are additive meta keys only — every existing key keeps
    its exact name and shape.
</must>
Reject:
<reject>
  - telemetry failure (missing stats, len overflow, absent meta) failing
    the capture or the turn path -> never an Err; counters degrade to
    absent keys
  - a second transcript file read for stats -> forbidden; stats come from
    the grader's single pass
  - report script writing ANYTHING to storage -> read-only (SCAN + GET
    only)
</reject>
After:
<after>
  - A live turn writes an injection capture whose meta shows how many
    chars/tokens Lunaris injected, and a feedback capture whose meta shows
    transcript size + tool count + per-memory verdicts — enough to compute
    "injected tokens vs turn size vs cited rate" per scope offline.
  - The report script prints that aggregation for a real scope against
    live Moon.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ `grade_turn_feedback` can surface stats without widening its return
    into the fail-open matrix awkwardly — confidence medium; if wrong the
    contract's 3-tuple return gets clumsy; cost: small refactor, contained
    in one private fn.
  - [x] rendered_context.len() at the spawn site is the true injected
    payload size (verified: render happens line 788, spawn 789).
  - [x] Moon episode values are JSON docs with `source` + `metadata`
    fields the script can filter on (verified in task-3 tests:
    find_turn_feedback_metadata parses exactly this shape).
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: injection capture carries token counters
  Given a prompt-phase recall that injects two memories rendering to N chars
  When respond_with_memories completes and the trace lands
  Then the lunaris:memory_injection meta has injected_chars == N
  And injected_tokens_est == N / 4
  And every pre-existing meta key (injection_id, phase, memory_ids) is unchanged

Scenario: feedback capture carries transcript stats when detector ran
  Given a turn_feedback request with the task-3 citation fixture transcript
  When capture_feedback completes
  Then meta.transcript_stats.file_bytes equals the fixture's byte length
  And meta.transcript_stats.tool_call_count == 2
  And meta.transcript_stats.final_text_chars > 0
  And detector == "ok" and the verdicts array is unchanged from task-3 behavior

Scenario: no transcript -> no stats, capture still written
  Given a turn_feedback request with transcript_path = None
  When capture_feedback completes
  Then meta has NO transcript_stats key
  And detector == "skipped_no_transcript" and the capture succeeded (fail-open)

Scenario: report aggregates a scope correctly
  Given parsed episode docs: 2 injection captures (tokens 100, 50) and
        2 feedback captures (one with 3 cited + 1 uncited verdicts and
        transcript_stats, one detector-skipped)
  When the pure aggregation function runs
  Then it reports injected_tokens_total == 150, turns == 2,
       cited == 3, uncited == 1, cited_rate == 0.75
  And a malformed episode doc in the input is skipped, not fatal

Scenario: report script is read-only
  Given the script's Moon interaction layer
  When it collects a scope's episodes
  Then it issues only SCAN/MGET-class reads (no write command in the module)
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
context.rs (all private symbols — wire format unchanged, meta additive):
  spawn_trace_injection(..., injected_chars: usize)          # new last param
  trace_injection(..., injected_chars: usize)                # new last param
    meta += { "injected_chars": <usize>,
              "injected_tokens_est": <usize = injected_chars / 4> }
  struct TranscriptStats { file_bytes: u64, tool_call_count: usize,
                           final_text_chars: usize }         # Serialize
  grade_turn_feedback(...) -> (Vec<MemoryVerdict>, &'static str,
                               Option<TranscriptStats>)      # Some iff "ok"
  capture_feedback: meta += { "transcript_stats": {..} }     # only when Some

transcript.rs:
  TurnTranscript += pub file_bytes: u64                      # set by
  read_turn_transcript (from metadata().len(); Default = 0)

scripts/context-savings-report.py  (new, stdlib + socket RESP like
  test-recovery.py):
  aggregate(episodes: list[dict]) -> dict                    # pure, tested
    { "injected_tokens_total", "injection_count", "turns",
      "tool_calls", "cited", "uncited", "cited_rate" | None }
  main: --scope <scope> --host --port ; SCAN lunaris:{scope}:episode:* ;
    read-only; prints the aggregate as aligned text + --json flag

Fixture reuse: crates/lunaris-hook/tests/fixtures/transcript_citation.jsonl
(no new fixture).
```

Status: FROZEN @ v1 — approved by Tin (standing auto-mode delegation for
this milestone; task added by explicit "yes" 2026-07-17).
Least-sure flag surfaced at freeze: [contract] the 3-tuple
`grade_turn_feedback` return — if the tuple reads poorly the builder may
be tempted to introduce a struct; the contract PERMITS collapsing the
tuple into a small private result struct with the same three fields, and
nothing else. Second flag: [scenario] the read-only assertion on the
script is structural (no write commands present), not a live-Moon proof —
accepted, matches the adapter-test precedent.

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every scenario above has exactly one discriminating test.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - injection_trace_carries_token_counters (context.rs tests): drive
    respond_with_memories via the existing seeded-scope harness; read the
    lunaris:memory_injection meta; assert injected_chars == rendered len
    and injected_tokens_est == len/4; assert legacy keys intact.
  - feedback_pass_records_transcript_stats (context.rs tests): task-3
    fixture; assert transcript_stats {file_bytes == fs::metadata len,
    tool_call_count == 2, final_text_chars > 0}; assert verdicts still 4.
  - feedback_pass_no_transcript_has_no_stats (context.rs tests): extend
    scenario asserts on the existing fail-open path — meta lacks
    transcript_stats, detector == skipped_no_transcript.
  - transcript_reader_reports_file_bytes (transcript.rs tests): fixture
    read → file_bytes == std::fs::metadata(fixture).len().
  - test_aggregate_counts (scripts/tests/test_context_savings_report.py):
    pure aggregate() over synthetic episode dicts per scenario 4, incl.
    malformed-doc skip and cited_rate None when no verdicts.
  - test_report_module_is_readonly (same file): import the script module,
    assert its RESP command surface contains only read commands (SCAN,
    GET/MGET, HELLO/AUTH/SELECT allowance list).
</test_plan>

Tests live in: `crates/lunaris-hook/src/context.rs` ·
`crates/lunaris-hook/src/transcript.rs` ·
`scripts/tests/test_context_savings_report.py` · MUST run red (missing
implementation) before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-hook/src/context.rs` ·
`crates/lunaris-hook/src/transcript.rs` ·
`scripts/context-savings-report.py` ·
`scripts/tests/test_context_savings_report.py`
Strategy (ordered batches): 1. transcript.rs file_bytes · 2. context.rs
counters + stats · 3. report script + python tests.
Safety rule (feature-specific): telemetry NEVER fails a capture; report
script NEVER writes to storage.
Constraints: do NOT change any test or the contract; no new crate deps;
python stdlib only.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build
- [ ] the green was EARNED, not gamed
- [ ] concurrency / timing of the risky operation is safe
- [ ] no exposed secrets, injection openings, or unexpected dependencies
- [ ] layering & dependencies follow CONVENTIONS.md
- [ ] a person reviewed and approved the change

### Build expectations — what "correct" looks like (fill BEFORE build)
- [ ] a seeded-scope injection capture's meta shows injected_chars equal to
      the rendered string's length — confirmed by the context.rs test
      reading the stored episode back
- [ ] the citation fixture feedback capture's meta shows
      transcript_stats.file_bytes == the fixture file's on-disk size —
      confirmed by fs::metadata in the test
- [ ] `python3 scripts/context-savings-report.py --help` runs and the pure
      aggregate() is import-tested without Moon — confirmed by the python
      suite

### Deep checks — do not skim
- [ ] WIRING (code) — injected_chars flows spawn-site → meta; stats flow
      grader → meta; script main() calls aggregate()
- [ ] DEAD-CODE (code) — no orphaned symbol
- [ ] SEMANTIC (prose) — MILESTONE task-10 a/b/c all covered; (c) A/B
      comparison explicitly out of scope

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
Reviewed by: <name> · date: <date>

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): injected_tokens_est per scope per day ·
cited_rate trend · transcript_stats presence rate (detector health).

### Spec delta

### Competency deltas
