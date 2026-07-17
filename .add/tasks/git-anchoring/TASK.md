# TASK: Git Anchoring

slug: git-anchoring · created: 2026-07-17 · stage: production
autonomy: auto
phase: tests   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> Milestone engram-soul-loop task 5: contextd stamps `git_head` + `files[]`
> metadata on every capture so the task-6 staleness pass can diff anchors
> vs HEAD. Backfill: none — new captures only.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-hook/src/context.rs`:
  - `ContextRequest::CaptureToolCall` / `CaptureToolResult` (lines 85-98)
    — carry `cwd/scope/session_id/tool/payload`, NO paths today.
  - `capture_tool` (~976) — shared writer for both capture variants;
    builds meta {session_id?, tool_name?, capture_kind}; called via
    `spawn_capture_tool` (fire-and-forget tokio::spawn).
  - `capture_lightweight` (~1169) — the SINGLE choke point every capture
    goes through (capture_tool, trace_injection, capture_feedback,
    session-digest trace). Signature: (scope, source, content, meta).
  - Dispatch match (~430-470) resolves scope from cwd then DROPS cwd —
    cwd must be threaded to stamp git_head.
- `scripts/lunaris-codex-hook-adapter.py`:
  - `run_capture` (~158) — builds capture_tool_call / capture_tool_result
    requests; `extract_paths(event)` (~721, capped at 12, dedupes,
    filters path-shaped keys) already exists and is used for
    `recall_after_tool` (`"paths": extract_paths(event)` line 267).
- `RecallAfterTool.paths: Option<Vec<String>>` (line 80) — the wire
  precedent for an optional paths list (serde default).
- No git plumbing exists anywhere in lunaris-hook (verified grep).
- Convention anchors: fail-open turn path; additive wire fields with
  `#[serde(default)]`; no lock across .await (parking_lot);
  env reads at call time (issue #49).

Anchors the contract cites: `capture_tool`, `capture_lightweight`,
`ContextRequest::{CaptureToolCall,CaptureToolResult}`, `run_capture`,
`extract_paths`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: git-anchor stamping on captures (`git_head` + `files[]`)

Framings weighed: stamp-at-the-choke-point (chosen — one implementation
in `capture_lightweight` callers can't drift) · per-caller stamping
(rejected: 4 call sites, task-1-style drift risk) · adapter-side git
resolution (rejected: contextd owns capture semantics; adapters stay
dumb).

Must:
<must>
  - New module `crates/lunaris-hook/src/git_anchor.rs`:
    `head_for_cwd(cwd: &Path) -> Option<String>` — resolves the repo
    HEAD (40-hex) for cwd via `git rev-parse HEAD` (tokio::process,
    hard timeout 300ms), returns None on ANY failure (no repo, no git,
    timeout, non-zero exit). Result cached per canonicalized cwd with a
    5s TTL in a `parking_lot::Mutex<HashMap>` — lock held only for
    sync map ops, NEVER across the subprocess await. Negative results
    cached too (a non-repo cwd must not fork git every capture).
  - `capture_lightweight` gains `cwd: Option<&Path>` as last param; when
    Some and `head_for_cwd` resolves, stamps meta `git_head: <hex>`.
    Existing meta keys unchanged. ALL FOUR callers thread their cwd
    (capture_tool/trace_injection/capture_feedback/digest trace get cwd
    threaded from their dispatch arms; where genuinely absent, None).
  - `ContextRequest::CaptureToolCall` and `CaptureToolResult` gain
    `#[serde(default)] paths: Option<Vec<String>>`; `capture_tool`
    stamps meta `files: [..]` when non-empty (post_tool file anchor).
  - Adapter `run_capture`: both capture payloads gain
    `"paths": extract_paths(event) or None` (strip_none drops absent).
  - Every stamp is fail-open: a git failure or absent paths never fails
    or delays a capture beyond the 300ms subprocess cap.
</must>
Reject:
<reject>
  - git resolution failure surfacing as a capture Err -> forbidden;
    meta simply lacks git_head
  - holding the cache lock across the git subprocess await -> forbidden
    (UNSAFE_POLICY lock discipline)
  - stamping files[] from server-side payload re-parsing -> forbidden;
    files come from the adapter's extract_paths on the wire (single
    extraction implementation)
  - unbounded cache growth -> entries expire by TTL check on access;
    map pruned opportunistically when > 64 entries
</reject>
After:
<after>
  - Every new tool capture in a git repo carries meta.git_head (and
    files[] when the event touched paths); prompt/feedback/injection
    captures carry git_head alone. The task-6 staleness pass can diff a
    memory's anchors against current HEAD without re-deriving anything.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ 5s TTL + per-cwd caching keeps git subprocess overhead negligible
    under capture bursts — confidence high-medium; if wrong (many
    distinct cwds), the 64-entry prune bounds memory and the 300ms cap
    bounds latency; cost: tuning, not redesign.
  - [x] adapter extract_paths is the right files[] source — verified: it
    already feeds RecallAfterTool.paths (recall precedent).
  - [x] capture_lightweight is the single choke point — verified callers.
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: tool capture in a git repo stamps head and files
  Given a temp git repo with one commit and a capture_tool_result request
        with cwd=<repo> and paths=["src/lib.rs"]
  When the capture lands
  Then the episode meta has git_head == the repo's HEAD (40-hex)
  And meta.files == ["src/lib.rs"]
  And all pre-existing meta keys are unchanged

Scenario: capture outside a git repo stamps neither, still succeeds
  Given cwd = a plain temp dir (no .git)
  When a capture_tool_call request lands
  Then the episode is written with NO git_head and NO files key

Scenario: paths absent -> no files key
  Given a capture in a git repo with no paths on the wire
  Then meta has git_head but NO files key

Scenario: head_for_cwd caches within TTL
  Given two head_for_cwd calls for the same cwd inside 5s
  Then the second returns the same value without a second subprocess
  (observable: cache entry count / a test seam counting invocations)

Scenario: feedback capture carries git_head
  Given a turn_feedback request with cwd=<repo>
  Then the lunaris:turn_feedback episode meta has git_head
  And detector/verdicts behavior is unchanged (task-3 tests stay green)

Scenario: old adapter wire (no paths key) still decodes
  Given a capture_tool_call JSON without the paths field
  Then it decodes and captures exactly as before
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
crates/lunaris-hook/src/git_anchor.rs (new):
  pub(crate) async fn head_for_cwd(cwd: &Path) -> Option<String>
    # tokio::process git rev-parse HEAD, 300ms tokio::time::timeout,
    # trim to 40-hex validate; None on any failure
  cache: parking_lot::Mutex<HashMap<PathBuf, (Instant, Option<String>)>>
    # TTL 5s (const TTL: Duration), prune when len > 64 (const CAP)
    # lock NEVER held across .await; Some/None both cached
  pub(crate) fn ttl_cache_len() -> usize   # test seam (cfg(test) ok)

context.rs:
  capture_lightweight(scope, source, content, meta, cwd: Option<&Path>)
    # stamps meta.git_head when cwd resolves; callers thread cwd:
    #   capture_tool + spawn_capture_tool (+ cwd param)
    #   trace_injection + spawn_trace_injection (+ cwd param)
    #   capture_feedback (has cwd in its dispatch arm)
    #   any other capture_lightweight caller -> None if no cwd in scope
  CaptureToolCall / CaptureToolResult += #[serde(default)]
    paths: Option<Vec<String>>
  capture_tool: meta.files = paths when Some(non-empty)

scripts/lunaris-codex-hook-adapter.py run_capture:
  both payloads += "paths": extract_paths(event) or None

Python test rides scripts/tests/test_adapter_feedback_transcript.py
pattern in a NEW file scripts/tests/test_adapter_capture_paths.py.
```

Status: FROZEN @ v1 — approved by Tin (standing auto-mode delegation;
milestone task 5 scope locked 2026-07-16).
Least-sure flag surfaced at freeze: [contract] threading cwd through the
two spawn_* fns changes private signatures the task-2/3 tests construct
directly — the builder may touch those tests' CALL ARGUMENTS (adding a
cwd arg) but MUST NOT weaken any assertion; flagged because "do not
change any test" gets a narrow, explicit exception here. Second: [spec]
5s TTL is a guess; correctness is unaffected (staleness of HEAD within
5s is indistinguishable from capture-time race anyway).

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: one discriminating test per scenario.
Plan:
<test_plan>
  - git_anchor.rs unit tests: resolves_head_in_temp_repo (init repo via
    std::process git, commit, assert 40-hex match), none_outside_repo,
    cache_hits_within_ttl (seam-count or cache_len based).
  - context.rs tests: tool_capture_stamps_head_and_files (scenario 1,
    temp repo + full request dispatch), capture_without_repo_omits_keys,
    paths_absent_omits_files, feedback_capture_carries_git_head
    (extends the existing harness; task-3 assertions untouched),
    old_wire_without_paths_decodes (serde_json round-trip test).
  - python: scripts/tests/test_adapter_capture_paths.py — run_capture
    forwards extract_paths output as "paths" on BOTH pre/post payloads;
    absent paths -> key omitted (strip_none).
</test_plan>

Tests live in: `crates/lunaris-hook/src/git_anchor.rs` ·
`crates/lunaris-hook/src/context.rs` ·
`scripts/tests/test_adapter_capture_paths.py` · MUST run red before
Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-hook/src/git_anchor.rs` ·
`crates/lunaris-hook/src/context.rs` · `crates/lunaris-hook/src/lib.rs` ·
`scripts/lunaris-codex-hook-adapter.py` ·
`scripts/tests/test_adapter_capture_paths.py`
Strategy: 1. git_anchor module · 2. capture_lightweight + threading ·
3. wire paths + capture_tool files · 4. adapter.
Safety rule: fail-open everywhere; lock never across .await; 300ms
subprocess cap.
Constraints: existing test ASSERTIONS untouched (call-argument updates
for new params allowed per the freeze flag); no new crate deps
(tokio::process is already in tokio).

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build (beyond the flagged
      call-argument exception)
- [ ] the green was EARNED, not gamed
- [ ] concurrency / timing of the risky operation is safe
- [ ] no exposed secrets, injection openings, or unexpected dependencies
- [ ] layering & dependencies follow CONVENTIONS.md
- [ ] a person reviewed and approved the change

### Build expectations — what "correct" looks like (fill BEFORE build)
- [ ] a real dispatch of capture_tool_result in a temp git repo lands an
      episode whose meta shows the repo's actual HEAD hash — confirmed
      by reading the episode back in the test
- [ ] a non-repo capture is byte-identical to today's meta — confirmed
      by the omits-keys test
- [ ] adapter sends paths on both capture types — confirmed by the
      python suite

### Deep checks — do not skim
- [ ] WIRING — git_anchor referenced from capture_lightweight; paths
      flow adapter → wire → meta
- [ ] DEAD-CODE — none
- [ ] SEMANTIC — MILESTONE task 5 covered; backfill explicitly none

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
Reviewed by: <name> · date: <date>

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch: git_head presence rate on new captures · head_for_cwd timeout
rate · cache size.

### Spec delta

### Competency deltas
