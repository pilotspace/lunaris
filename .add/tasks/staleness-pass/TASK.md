# TASK: Staleness Pass

slug: staleness-pass · created: 2026-07-17 · stage: production
autonomy: auto
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> Milestone engram-soul-loop task 6: diff memory git-anchors (task 5's
> meta.git_head/files) vs current HEAD → verify-agenda entries; decay +
> ⚠-banner stale-anchored hits in the inject. Task 7 (MCP
> verify_agenda/resolve tools) consumes the agenda — keep its JSON shape
> stable.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

(Explore dossier 2026-07-17 — all file:line anchors verified.)

- **Metadata is DISCARDED at hydration**: `hydrate.rs:141` deserializes
  the full `Episode` (whose `metadata` map carries git_head/files) but
  keeps only `(source, closed)`; `Hit` never carries metadata. Recall
  hits therefore cannot see anchors without a separate episode fetch.
- **ContextMemory** (`context.rs:191-197`): {episode_id, source, score,
  snippet} — episode_id is the string ULID; the natural place for a
  `stale` flag (precedent: `Hit.degraded: bool`, types.rs:103-107).
- **finish_recall** (`context.rs:799-833`): the shared post-curation /
  pre-render tail for ALL recall paths (prompt, post_tool,
  session_start); receives cwd since task 5. Per-memory render line:
  `context.rs:1466-1476`.
- **SessionDigest arm** (`context.rs:474-519`): has cwd; `build_digest`
  → `recent_by_source` (`lunaris/src/digest.rs:27-63`) holds FULL
  `Episode` objects (metadata intact) — the sweep's natural home.
  Scan-cap precedent: handover.rs `SCAN_CAP = 10_000` warn-and-partial.
- **BoostProvider seam is the WRONG decay point**: `priors(scope, ids)`
  receives hydrated chunk/fact ids, NOT episode ids (builder.rs:451-454)
  — a staleness provider would need chunk→episode indirection. Hook-side
  curation has episode_id directly.
- **Keyspace pattern** (`lunaris-core/src/keyspace.rs:164-178`,
  activation template): add `verify_agenda_key/verify_agenda_prefix`
  (`lunaris:{scope}:verify_agenda:{ulid}`) — helpers MUST live in
  lunaris-core (RC-1).
- **Ledger-writer precedent**: `ScopedLunaris::record_activation_refs`
  (`handle.rs:~1511`) — grouped RMW, one atomic_write, engine-level.
  Mirror for agenda upserts.
- **git_anchor** (task 5): `head_for_cwd` TTL cache; same
  fail-open/timeout/caching pattern to extend with a changed-files
  helper.
- **Commit detection**: no classifier exists anywhere; adapter forwards
  the raw event as `payload`; the shell command text is inside
  `tool_input.command`-shaped keys. Adapter-side classification is the
  cheap v1.
- No existing agenda/banner/stale symbols to collide with (verified).

Anchors the contract cites: `finish_recall`, `curate_context_memories`,
`render_context`, `build_digest`, `recent_by_source`,
`ScopedLunaris::record_activation_refs`, `git_anchor::head_for_cwd`,
`keyspace::activation_key`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: staleness assessment on inject + verify-agenda persistence

Framings weighed: hook-side post-curation assessment (chosen — bounded
point reads on the ≤max_hits FINAL list; episode_id in hand; banner and
decay live where rendering lives) · BoostProvider decay (rejected:
chunk-id/episode-id mismatch, dossier Q6) · hydrate-time metadata
threading (rejected for v1: touches Hit wire shape across 3 crates;
recorded as a spec delta for deeper decay-below-the-cut).

Must:
<must>
  - `lunaris-core/src/keyspace.rs`: `verify_agenda_key(scope, id)` +
    `verify_agenda_prefix(scope)` per the activation template. The
    agenda entry's ULID IS the episode's ULID (natural idempotent key).
  - `lunaris-hook/src/git_anchor.rs` gains
    `changed_files_since(cwd, anchor_head) -> Option<HashSet<String>>`:
    `git diff --name-only <anchor>..HEAD` (300ms cap, fail-open None),
    TTL-cached per (canonical cwd, anchor_head) in the same style as
    head_for_cwd; at most 8 DISTINCT anchor diffs resolved per sweep
    call site (excess anchors skipped fail-open — caught next sweep).
  - New `lunaris-hook/src/staleness.rs`:
    `pub(crate) struct StaleVerdict { pub stale: bool }` +
    `assess(meta: &Map, current_head: &str, changed: &dyn Fn(&str) ->
    Option<HashSet<String>>) -> StaleVerdict` (pure given the closure):
    stale iff meta.git_head is a 40-hex string != current_head AND
    meta.files is a non-empty array intersecting
    changed(anchor_head) (None from the closure -> NOT stale,
    fail-open). No git_head or no files -> NOT stale.
    `pub(crate) const STALE_DECAY: f32 = 0.7;`
  - `ContextMemory` gains `#[serde(default)] pub stale: bool`.
  - `finish_recall`: after curation, for the curated list (≤ max_hits)
    read each episode doc (read_as_of + episode_key, bounded, HlcClock
    tick per the task-2 provider pattern), assess staleness (resolve
    HEAD once via head_for_cwd(cwd)); on stale: set `stale=true`,
    `score *= STALE_DECAY`, re-sort the curated list by the existing
    ordering criteria. Any read/git failure -> memory stays fresh
    (fail-open, never blocks the inject).
  - `render_context` per-memory line: stale memories render with the
    marker `⚠ code changed since` appended inside the bracket header
    (exact form: `- [source=.. score=.. id=.. ⚠ code-changed] snippet`).
  - SessionDigest arm: after build_digest, run the agenda sweep —
    scan episodes (SCAN_CAP 5_000, warn-and-partial per handover
    precedent), assess each episode WITH a git_head meta, and upsert
    stale ones via new `ScopedLunaris::upsert_verify_agenda(&[entry])`
    (ONE atomic_write batch, record_activation_refs pattern). Entry
    JSON (task-7 wire contract, keep stable):
    `{episode_id, anchor_head, current_head, files: [changed∩anchored],
    first_seen_ms, last_seen_ms, v: 1}` — first_seen preserved on
    upsert (RMW read first). Sweep is spawned fire-and-forget
    (spawn_capture_tool pattern) — never delays the digest response.
  - Adapter `run_capture` posttooluse: when the event's command text
    (tool_input.command / toolInput.command) matches `git commit` as a
    standalone token sequence, add `"commit": True` to the request;
    `CaptureToolResult` gains `#[serde(default)] commit: bool`; a
    commit-shaped capture spawns the SAME agenda sweep for its scope
    (shared fn with the digest arm).
</must>
Reject:
<reject>
  - staleness/agenda failure surfacing as a recall/digest/capture Err ->
    forbidden; every failure path degrades to fresh/no-sweep
  - a second full-episode scan on the RECALL path -> forbidden; recall
    assessment reads ONLY the curated hits' episodes (≤ max_hits point
    reads)
  - agenda writes via raw storage.atomic_write in lunaris-hook ->
    forbidden; the writer is ScopedLunaris::upsert_verify_agenda
    (engine-level, keyspace helpers from lunaris-core — RC-1)
  - unbounded git subprocesses in a sweep -> ≤ 8 distinct anchor diffs
    + TTL cache; excess skipped
  - marking stale on HEAD-mismatch alone (no files overlap) ->
    forbidden (mass-flag noise); no-anchor memories are never stale
</reject>
After:
<after>
  - Editing an anchored file visibly decays + ⚠-banners that memory in
    the next inject (the milestone exit criterion, E2E-tested); a
    SessionStart (or commit capture) writes agenda entries the task-7
    tools can list and resolve; fresh memories render byte-identically
    to today.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Post-curation decay cannot demote a stale hit BELOW the curation
    cut (a stale hit that made top-k stays injected, just decayed +
    bannered) — accepted for v1; the banner is the primary signal and
    hydrate-time threading is the recorded follow-up. Cost if wrong:
    stale hits occupy inject slots; mitigated by task-7 resolve.
  - [x] curated list ≤ max_hits keeps recall-path point reads bounded
    (verified: curation truncates to max_hits).
  - [x] episode ULID as agenda key gives idempotent upserts (natural
    dedupe, verified key format).
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: stale memory decays and banners in the inject (EXIT CRITERION)
  Given a temp git repo at commit A and an episode anchored
        {git_head: A, files: ["src/lib.rs"]} whose content matches a query
  And src/lib.rs is modified and committed as B
  When a prompt-phase recall runs with cwd=<repo>
  Then that memory's ContextMemory has stale == true
  And its score is decayed by STALE_DECAY and the list re-sorted
  And the rendered line contains "⚠ code-changed"

Scenario: anchored but untouched memory stays fresh
  Given the same repo where HEAD moved A -> B but the anchored file was
        NOT part of the diff
  Then stale == false and the rendered line is byte-identical to today

Scenario: unanchored memory is never stale
  Given an episode with no git_head meta
  Then stale == false, no episode-fetch failure can flag it

Scenario: git/read failure fails open
  Given head_for_cwd returns None (non-repo cwd)
  Then all memories render fresh and the recall response is Ok

Scenario: session digest writes agenda entries
  Given a scope with one stale-anchored and one fresh-anchored episode
  When the SessionDigest arm runs with cwd=<repo>
  Then exactly one verify_agenda KV entry exists, keyed by the stale
       episode's ULID, with {anchor_head: A, current_head: B,
       files: ["src/lib.rs"], v: 1}
  And a second digest run preserves first_seen_ms (upsert)

Scenario: commit-shaped capture triggers the sweep
  Given an adapter posttooluse event whose tool_input.command is
        "git commit -m x"
  Then the adapter request carries commit: true
  And the contextd capture spawns the agenda sweep (same entries as the
       digest sweep)

Scenario: non-commit Bash events do not sweep
  Given command "git log" or "echo commit"
  Then commit is absent/false and no sweep is spawned

Scenario: old wire without commit/stale fields decodes
  Given pre-task-6 JSON for CaptureToolResult and ContextResponse
  Then serde round-trips exactly as before
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
lunaris-core:
  keyspace: verify_agenda_key(scope, Ulid) / verify_agenda_prefix(scope)
  (activation template; kind string "verify_agenda")

crates/lunaris/src/handle.rs (ScopedLunaris):
  pub struct VerifyAgendaEntry { pub episode_id: Ulid,
    pub anchor_head: String, pub current_head: String,
    pub files: Vec<String>, pub first_seen_ms: u64,
    pub last_seen_ms: u64, pub v: u32 }        # Serialize+Deserialize
  pub async fn upsert_verify_agenda(&self, entries: &[VerifyAgendaEntry])
    -> Result<(), Error>   # RMW: read existing -> preserve first_seen_ms
                           # -> ONE atomic_write of KvPuts

lunaris-hook/src/git_anchor.rs:
  pub(crate) async fn changed_files_since(cwd: &Path, anchor: &str)
    -> Option<HashSet<String>>   # git diff --name-only anchor..HEAD,
                                 # 300ms cap, TTL cache (cwd, anchor)
  pub(crate) const MAX_ANCHOR_DIFFS_PER_SWEEP: usize = 8;

lunaris-hook/src/staleness.rs (new):
  pub(crate) const STALE_DECAY: f32 = 0.7;
  assess(...) per §1 Must (pure given the changed-files closure)
  sweep_and_upsert(service-ish deps, scope, cwd) — shared by digest arm
    + commit-capture arm; SCAN_CAP 5_000; spawned fire-and-forget

lunaris-hook/src/context.rs:
  ContextMemory += #[serde(default)] pub stale: bool
  finish_recall: post-curation assessment + decay + re-sort + banner
    (resolve HEAD once; ≤ max_hits read_as_of point reads; fail-open)
  render_context: stale line form
    "- [source=<s> score=<f> id=<id> ⚠ code-changed] <snippet>"
  CaptureToolResult += #[serde(default)] commit: bool -> spawns sweep

scripts/lunaris-codex-hook-adapter.py run_capture (posttooluse):
  command text from tool_input.command | toolInput.command; regex
  \bgit\s+commit\b -> "commit": True (absent otherwise)
  python test file: scripts/tests/test_adapter_commit_detect.py
```

Status: FROZEN @ v1 — approved by Tin (standing auto-mode delegation;
milestone task 6 scope + stale policy locked 2026-07-16).
Least-sure flag surfaced at freeze: [spec] post-curation decay can't
demote below the curation cut — v1 accepts a stale hit holding an
inject slot (banner is the primary signal); hydrate-time metadata
threading recorded as the follow-up spec delta. Second: [contract]
`⚠ code-changed` inside the bracket header — chosen over a separate
banner line to keep the injected block line-per-memory parseable by the
task-3 transcript reader (whose `- [` line parser must keep matching —
verify the citation detector still parses stale lines: the header
tail after id= is split on whitespace, so the appended marker must not
break `id=` extraction; scenario 1's E2E test must assert the line
still round-trips through transcript.rs parse_injection_line).

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: one discriminating test per scenario.
Plan:
<test_plan>
  - keyspace tests: verify_agenda key/prefix format (mirror activation).
  - handle.rs test (memory:// engine): upsert_verify_agenda writes one
    atomic batch; second upsert preserves first_seen_ms, updates
    last_seen_ms.
  - git_anchor tests: changed_files_since returns the touched file in a
    temp repo (commit A, edit, commit B); None outside repo; cached
    within TTL.
  - staleness.rs unit tests: assess truth table (stale / untouched-file
    fresh / no-anchor fresh / closure-None fresh).
  - context.rs tests: EXIT-CRITERION E2E (temp repo, seeded anchored
    episode via the existing seeded-scope harness, prompt recall →
    stale flag + decayed order + "⚠ code-changed" in rendered +
    parse_injection_line still extracts the id from the stale line);
    fresh-path byte-identical render; digest-arm agenda entry write +
    first_seen preservation; commit-capture spawns sweep (commit: true
    → agenda entry exists after settle); old-wire serde round-trip.
  - python: test_adapter_commit_detect.py — "git commit -m x" → commit
    True; "git log"/"echo commit" → absent (strip_none).
</test_plan>

Tests live in: `crates/lunaris-core/src/keyspace.rs` ·
`crates/lunaris/src/handle.rs` (or tests/) ·
`crates/lunaris-hook/src/git_anchor.rs` ·
`crates/lunaris-hook/src/staleness.rs` ·
`crates/lunaris-hook/src/context.rs` ·
`scripts/tests/test_adapter_commit_detect.py` · MUST run red before
Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-core/src/keyspace.rs` ·
`crates/lunaris/src/handle.rs` · `crates/lunaris-hook/src/`
(git_anchor.rs, staleness.rs new, context.rs, lib.rs) ·
`scripts/lunaris-codex-hook-adapter.py` ·
`scripts/tests/test_adapter_commit_detect.py`
Strategy: 1. keyspace + engine writer · 2. git_anchor diff helper ·
3. staleness module · 4. recall-path assessment + render · 5. sweep
wiring (digest + commit) · 6. adapter.
Safety rule: fail-open everywhere; the recall path adds ≤ max_hits
point reads + ≤ 1 git subprocess (cached); locks never across .await.
Constraints: existing test ASSERTIONS untouched (call-argument updates
allowed only where new params force them); no new crate deps.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build (beyond flagged
      call-argument exception)
- [ ] the green was EARNED, not gamed
- [ ] concurrency / timing of the risky operation is safe
- [ ] no exposed secrets, injection openings, or unexpected dependencies
- [ ] layering & dependencies follow CONVENTIONS.md
- [ ] a person reviewed and approved the change

### Build expectations — what "correct" looks like (fill BEFORE build)
- [ ] the E2E exit-criterion test shows a real edited-file memory
      decayed + bannered in a real rendered inject — confirmed by
      reading the rendered block in the test
- [ ] a digest run against a stale scope leaves a readable
      verify_agenda KV whose JSON matches the task-7 wire shape —
      confirmed by read_as_of in the test
- [ ] the stale render line still parses through the task-3
      transcript reader — confirmed by the round-trip assertion

### Deep checks — do not skim
- [ ] WIRING — assessment invoked from finish_recall on the REAL
      prompt path; sweep invoked from BOTH digest and commit arms
- [ ] DEAD-CODE — none
- [ ] SEMANTIC — MILESTONE task 6 covered: sweep triggers, decay,
      banner; task-7 wire shape documented

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
Reviewed by: <name> · date: <date>

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch: stale-flag rate per inject · agenda size per scope · sweep
partial-scan warns · anchor-diff cache hit rate.

### Spec delta
- [SPEC · open] hydrate-time metadata threading so staleness decay can
  demote below the curation cut (evidence: §1 ⚠ assumption).

### Competency deltas
