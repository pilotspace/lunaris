# TASK: /dream skill + SessionStart distillation nudge (agenda-size threshold)

slug: dream-skill · created: 2026-07-18 · stage: production
autonomy: auto
phase: done
<!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> engram-soul-loop **task 9** — the LAST task, closes the milestone. MILESTONE.md lines 114-115:
> "`/dream skill + stop-hook nudge`: skill drives agenda→distill→resolve; Stop hook computes
> agenda size, SessionStart injects the nudge line when over threshold." v1 = manual `/dream` +
> threshold nudge; v2 (cron autonomous + session-end piggyback) = doc-stubs behind env flags.
> **SEQUENCING: delegate only AFTER 8b (distill) merges** — touches `context.rs` (shared with 8b).
> Depends on 8a `memory.dream_agenda` + 8b `memory.distill` + task-7 `memory.resolve` (all in roster).

---

## 0 · GROUND — the real codebase (from task-9 scout dossier)

Touches (files · symbols · signatures):
- `crates/lunaris-hook/src/context.rs:504-555` — `ContextRequest::SessionDigest` arm (SessionStart). Calls `build_digest` then `finish_recall`. **THE nudge injection point.** `finish_recall` (`context.rs:839-880`) SHORT-CIRCUITS at 849-851 (`if memories.is_empty() { return Ok(ContextResponse::empty()) }`) — so the nudge MUST be spliced into `resp.rendered_context` in the SessionDigest arm AFTER `finish_recall` returns, synthesizing a minimal wrapper block when the response came back empty. Do NOT model the nudge as a synthetic `ContextMemory` (it would drag through assess_staleness / trace_injection citation ids / activation-ref recording — none valid for a non-episode line).
- `crates/lunaris-hook/src/context.rs:215-240` — `ContextResponse { ok, injection_id, memories, rendered_context: String, lsn, error }` + `::empty()`. `emit_context_response` (`scripts/lunaris-codex-hook-adapter.py:562-583`) forwards `rendered_context` verbatim as `hookSpecificOutput.additionalContext` — no re-parse. A plain nudge line appended to `rendered_context` reaches the model.
- `crates/lunaris-consolidate/src/ledger_reference_source.rs:34-53` — `LedgerReferenceSource::scan(scope) -> Vec<(Ulid, ActivationRecord)>`. **The cheap agenda-size source**: one `scan_range` over `activation_prefix`; count `!record.is_archived()` (`activation.rs:191`). O(ledger rows), NO per-candidate episode hydrate, NO fact scan, NO Leiden. Over-counts slightly (doesn't exclude `distilled:*`/gone episodes — acceptable for a threshold nudge). `dream.rs`/`build_dream_agenda` is frozen READ-ONLY + heavy (full hydrate+facts+Leiden) — do NOT call it for the nudge.
- `crates/lunaris-hook/src/context.rs:1983-1998` — `env_usize_any(&[names]) -> Option<usize>`, `env_flag(name) -> bool`. Env pattern to follow. `DEFAULT_DIGEST_MAX_HITS`/`_CHARS` consts at `context.rs:31-32` — add `DEFAULT_DREAM_NUDGE_THRESHOLD` alongside.
- `crates/lunaris-hook/src/context.rs:1258-1269` — `spawn_agenda_sweep` fire-and-forget precedent (NOT needed for compute-on-read design, but the shape to know).
- `.claude/skills/add/SKILL.md:1-23` — the ONLY existing skill; frontmatter shape to mirror for `.claude/skills/dream/SKILL.md` (`name, description, user-invocable, when_to_use, category, keywords, argument-hint, license, metadata.author/version`), body = plain Markdown.
- `crates/lunaris-mcp/tests/server_boot.rs:41-43` — `memory.resolve`, `memory.dream_agenda`, `memory.distill` all in roster + callable by name from a skill (`main.rs:497-568`).
- Wiring (real path, no code change needed): SessionStart hook → `scripts/lunaris-codex-hook-adapter.py --mode session-start` → `{"type":"session_digest"}` → `SessionDigest` arm. (`scripts/setup-lunaris-agents.py:552,566` registers it; 8s budget — a single ledger scan fits.)

Context (working folder): `.add/tasks/dream-skill/`. Milestone lines 26-28, 114-115.

Honors: env pattern via `env_usize_any`/`env_flag`; no lock across await; nudge computation must fail-open (a scan error → NO nudge, never blocks/breaks the digest). The nudge line must NOT carry an `id=` token (that token is load-bearing for `transcript::parse_injection_line`/citation detector — a nudge is not a citable memory). SKILL.md is a doc, not code.

Anchors: `LedgerReferenceSource::scan`, `ActivationRecord::is_archived`, `ContextResponse::rendered_context`, `env_usize_any`, SessionDigest arm.

---

## 1 · SPECIFY — the rules

Feature: a SessionStart nudge — "N memories are ripe for distillation — run /dream" — injected when the pending-distillation agenda size is over a threshold, plus a `/dream` skill that drives the harness through `memory.dream_agenda → distill → resolve`.

Framings weighed:
- **Compute-on-read at SessionStart (chosen)** — count non-archived ledger candidates synchronously in the SessionDigest arm; no Stop-side state, no marker file, no new keyspace, non-racy (nothing writes between Stop and next SessionStart, so the count is identical to a Stop-computed one). Slight reinterpretation of the milestone's literal "Stop hook computes" — flagged; observable behavior identical.
- Stop-computes-to-marker-file (à la `session_marker.rs`) — matches the literal text but adds IO surface + a racy fire-and-forget handoff (Stop's spawn may not finish before next SessionStart). Rejected for v1.
- New Lunaris KV keyspace counter row — most complex, requires a Stop-path write outside frozen `dream.rs`. Rejected.

Must:
<must>
  - In the SessionDigest arm, AFTER `finish_recall`, compute agenda_size = count of `LedgerReferenceSource::scan(scope)` rows with `!is_archived()`. Fail-open: any scan error → agenda_size unavailable → NO nudge (never error the digest).
  - threshold = `env_usize_any(&["LUNARIS_DREAM_NUDGE_THRESHOLD"]).unwrap_or(DEFAULT_DREAM_NUDGE_THRESHOLD)`.
  - If agenda_size >= threshold (and threshold > 0), append a single nudge line to `resp.rendered_context`: `"⟳ {N} memories are ripe for distillation — run /dream to consolidate."` — NO `id=` token, NOT a ContextMemory. When `finish_recall` returned `ContextResponse::empty()` (no digest memories), synthesize a minimal `<lunaris_memory_context phase="session_start">…</lunaris_memory_context>`-style wrapper (or a bare line) carrying ONLY the nudge, and set `resp.ok = true` so the adapter forwards it.
  - Below threshold, or agenda_size == 0, or threshold == 0: NO nudge, response byte-identical to today.
  - Ship `.claude/skills/dream/SKILL.md`: frontmatter mirrors `add/SKILL.md`; body instructs the harness to (1) call `memory.dream_agenda` to list clusters, (2) for each cluster worth consolidating, author distilled prose and call `memory.distill(kind, content, source_episode_ids)`, (3) call `memory.resolve` for anything judged stale/superseded. v2 cron + session-end-piggyback documented as behind `LUNARIS_DREAM_CRON` / `LUNARIS_DREAM_PIGGYBACK` env flags but NOT wired (doc-stub only).
  - The nudge computation adds at most one ledger prefix scan to SessionStart (within the 8s budget); no per-row episode hydrate.
</must>
Reject:
<reject>
  - A ledger-scan failure during nudge computation → swallow + NO nudge (fail-open) — never surface an error or empty the digest that would otherwise have rendered.
  - `LUNARIS_DREAM_NUDGE_THRESHOLD` unparseable/absent → default (never panic).
</reject>
After:
<after>
  - With ≥threshold non-archived candidates, a fresh SessionStart digest's `rendered_context` contains the nudge line (even when there are zero digest memories).
  - With <threshold, the digest is byte-identical to the pre-task-9 output (no nudge).
  - `.claude/skills/dream/SKILL.md` exists, is user-invocable, and names the three real tools.
  - The nudge line has no `id=` token (never mis-parsed as a citable memory by the citation detector).
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ **Compute-on-read at SessionStart is an acceptable reading of "Stop hook computes agenda size"** — lowest confidence because it deviates from the literal milestone wording; chosen because the count is identical either way (no writes between Stop and next SessionStart), it is non-racy, and it avoids new IO/keyspace surface. If wrong (Tin wanted the number literally stamped at Stop, e.g. for a mid-session or cron consumer): the compute moves to a Stop-side marker-file write (session_marker.rs precedent) — a localized change, no contract-shape impact. Flagged per blueprint-canonical.
  - [ ] Cheap candidate-count (non-archived ledger rows, not true Leiden clusters) is an acceptable "agenda size" for the nudge — confirm approximate ripe-count is fine; the true cluster breakdown comes from /dream itself.
  - [ ] DEFAULT_DREAM_NUDGE_THRESHOLD = 5 is a sane default.
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases

<scenarios>

```gherkin
Scenario: nudge injected when agenda over threshold, even with an empty digest
  Given a scope with 6 non-archived activation-ledger candidates and NO decision:-prefixed digest memories
  And LUNARIS_DREAM_NUDGE_THRESHOLD unset (default 5)
  When a SessionStart session_digest is handled
  Then rendered_context is non-empty and contains "ripe for distillation — run /dream"
  And ok == true
  And the nudge line carries no "id=" token

Scenario: no nudge below threshold — digest byte-identical
  Given a scope with 2 non-archived candidates (threshold 5)
  When a SessionStart session_digest is handled
  Then rendered_context is exactly what it would be without task 9 (no nudge line)

Scenario: archived candidates do not count toward the agenda size
  Given a scope with 5 candidates of which 3 are archived (is_archived)
  And threshold 5
  When a SessionStart session_digest is handled
  Then agenda_size == 2 < 5 and NO nudge is injected

Scenario: ledger scan failure fails open
  Given the ledger scan errors
  When a SessionStart session_digest is handled
  Then the digest still returns (its normal memories/rendered_context) and NO nudge and NO error

Scenario: /dream skill exists and names the loop tools
  Given the repo
  When .claude/skills/dream/SKILL.md is read
  Then it is user-invocable and references memory.dream_agenda, memory.distill, and memory.resolve
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape

```
SessionStart nudge (in-process, lunaris-hook::context SessionDigest arm — NO new wire DTO):
  After finish_recall(...):
    let threshold = env_usize_any(&["LUNARIS_DREAM_NUDGE_THRESHOLD"]).unwrap_or(DEFAULT_DREAM_NUDGE_THRESHOLD /*=5*/);
    if threshold > 0 {
       if let Ok(refs) = LedgerReferenceSource::new(storage).scan(&scope).await {
           let n = refs.iter().filter(|(_,r)| !r.is_archived()).count();
           if n >= threshold { splice_nudge(&mut resp, n); }   // append line to rendered_context; ensure resp.ok
       }  // Err -> fail open, no nudge
    }
  splice_nudge: append "⟳ {n} memories are ripe for distillation — run /dream to consolidate." to
    resp.rendered_context; if resp.rendered_context.is_empty() wrap it in a minimal
    <lunaris_memory_context phase="session_start"> … </lunaris_memory_context> block; set resp.ok = true.
    NO "id=" token on the nudge line.
  New const: DEFAULT_DREAM_NUDGE_THRESHOLD: usize = 5 (context.rs:31-32 neighbourhood).
  New env: LUNARIS_DREAM_NUDGE_THRESHOLD (usize). v2 doc-only: LUNARIS_DREAM_CRON, LUNARIS_DREAM_PIGGYBACK.

Skill: .claude/skills/dream/SKILL.md
  frontmatter { name: dream, description, user-invocable: true, when_to_use, category: workflows,
                keywords: [dream, distill, consolidate, memory], argument-hint, license: MIT,
                metadata: { author, version } }
  body: drives memory.dream_agenda -> (harness authors distilled prose) -> memory.distill ->
        memory.resolve; v2 cron/piggyback documented as env-gated stubs.

Access: READ-ONLY at SessionStart (one extra ledger prefix scan). No writes. No new keyspace/marker.
```

Status: FROZEN @ v1 — approved by Tin Dang (autonomous project-lead, engram-soul-loop standing directive)

**Lowest-confidence flag at freeze [spec]:** compute-on-read at SessionStart vs the literal "Stop hook computes" (⚠ §1). Accepted: identical value, non-racy, no new IO/keyspace surface; a Stop-side marker-file variant is a localized fallback if the literal stamping is later required.

---

## 4 · TESTS — failing-first suite (red)

Coverage target: 90% of the nudge branch (threshold met / below / archived-excluded / fail-open / empty-digest-wrapper).
Plan:
<test_plan>
  - test_nudge_injected_over_threshold_empty_digest: seed N≥threshold non-archived ledger records via record_activation_refs, no digest memories → session_digest → rendered_context contains the nudge, ok==true, no "id=".
  - test_no_nudge_below_threshold_byte_identical: N<threshold → rendered_context == the no-task-9 baseline (capture baseline by threshold=0 or high env).
  - test_archived_excluded_from_agenda_size: mix archived + live records → count excludes archived → no nudge when live < threshold.
  - test_ledger_scan_failure_fails_open: inject a failing storage → digest still returns, no nudge, no error.
  - test_threshold_env_override: LUNARIS_DREAM_NUDGE_THRESHOLD parsing + default.
  - test_dream_skill_file_shape: read .claude/skills/dream/SKILL.md → user-invocable frontmatter + references memory.dream_agenda/distill/resolve (a repo-file assertion test).
</test_plan>
Tests live in: `crates/lunaris-hook/src/context.rs` (nudge unit tests, StubEmbedder where recall involved), a small skill-file assertion test (in lunaris-hook or a repo test). MUST run red before Build.

---

## 5 · BUILD — AI writes code

Scope (may touch): `crates/lunaris-hook/src/context.rs` `.claude/skills/dream/SKILL.md` (new) `docs/integration/hooks.md` (optional v2-stub note)
Strategy: 1. RED nudge tests + skill-file test. 2. GREEN nudge splice in SessionDigest arm + const + env + splice_nudge helper. 3. Write SKILL.md. 4. v2 doc-stub note.
Safety rule: fail-open — a nudge-compute error NEVER breaks the digest. No lock across await. No new writes/keyspace.
Constraints: do NOT change tests or contract; reuse env_usize_any; nudge line carries no id= token.

---

## 6 · VERIFY — evidence + non-functional review

- [ ] all tests pass; coverage held; no test/contract altered
- [ ] green EARNED — nudge appears over threshold + empty digest (discriminating), byte-identical below threshold, archived excluded, fail-open proven with a failing storage stub
- [ ] fail-open: a ledger-scan error yields the normal digest + no nudge + no error (tested)
- [ ] nudge line has NO id= token (would mis-trigger the citation detector) — asserted
- [ ] no lock across await; no new writes at SessionStart (still read-only)
- [ ] SKILL.md references the three real roster tools + is user-invocable

### Build expectations
- [ ] SessionStart with ≥threshold non-archived candidates shows the nudge even with zero digest memories — confirmed by test
- [ ] below threshold → digest unchanged — confirmed by byte-identical test
- [ ] .claude/skills/dream/SKILL.md drives dream_agenda→distill→resolve — confirmed by reading it

### Deep checks
- [ ] WIRING — splice_nudge called from SessionDigest arm; LedgerReferenceSource::scan reused (not build_dream_agenda)
- [ ] DEAD-CODE — none
- [ ] SEMANTIC — SKILL.md read in full: correct tool names/params, honest v1-vs-v2 scope

### GATE RECORD
Outcome: PASS
Evidence (orchestrator re-verified):
- lunaris-hook 111/111 green (105 + 6 new): test_nudge_injected_over_threshold_empty_digest (nudge lands even when finish_recall short-circuits to empty; ok==true; asserts NO id= token), test_no_nudge_below_threshold_byte_identical, test_archived_excluded_from_agenda_size, test_ledger_scan_failure_fails_open (ActivationScanFailingStorage → digest still renders, no nudge, no error), dream_nudge_threshold_env_wired_with_default_five, test_dream_skill_file_shape.
- Nudge is fail-open: SessionDigest arm scan Err → debug log + skip (context.rs:577-583); threshold-guarded; counts !is_archived(); uses cheap LedgerReferenceSource::scan (NOT build_dream_agenda). READ-ONLY, no new keyspace/marker.
- splice_dream_nudge: no id= token; wraps in a minimal <lunaris_memory_context> block when empty; sets ok=true.
- SKILL.md read in full: accurate to the real tool contracts (dream_agenda read-only, distill archive≠tombstone, resolve invalidate/supersede), honest v1-vs-v2 doc-stubs; fixed one cosmetic when_to_use emoji (⏳→⟳) to match the actual nudge line.
- .gitignore narrowing verified SAFE: ONLY .claude/skills/dream/SKILL.md tracked; no settings.local.json/secrets.
- cargo clippy --workspace --all-targets -D warnings clean (1m38s); fmt applied; vendor/moon pin restored.
Reviewed by: Tin Dang (autonomous project-lead, adversarial orchestrator re-verify) · date: 2026-07-18

---

## 7 · OBSERVE — feed the next loop

Watch: nudge fire rate; distill calls following a nudged session (did the nudge drive consolidation?).

### Spec delta
- [SPEC · open] v2 dream triggers — cron-autonomous (`LUNARIS_DREAM_CRON`) + session-end piggyback (`LUNARIS_DREAM_PIGGYBACK`), wired to real scheduling (v1 ships doc-stubs).
- [SPEC · open] true cluster-count nudge (call build_dream_agenda) if the approximate candidate-count proves misleading.
- [SPEC · open] Stop-side agenda-size stamping (marker file) if a mid-session/cron consumer needs the number literally at Stop.

### Competency deltas
- [ADD · open] closing engram-soul-loop: capture→distill→ground→recall→reinforce→decay/invalidate is fully wired; the harness-as-distiller decision held across all 10 tasks (evidence: dream wave shipped with zero Lunaris-internal distillation LLM).
