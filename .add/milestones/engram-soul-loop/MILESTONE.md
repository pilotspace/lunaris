# Milestone: engram-soul-loop

**Status: DRAFT — register via `add.py new-milestone --await-confirm` after
moon-v080-bump closes.** Decisions locked with Tin 2026-07-16 (interview x2).

## Goal

Give Lunaris a soul: close the open loop `capture → store → recall` into
`capture → distill → ground → recall → reinforce → decay/invalidate`, so an
AI coding agent's memory **improves with project age instead of degrading**.
Strategy pillar order: A (reinforcement) + B (git-grounded re-verify) +
C (harness-driven dreaming) in one milestone — Tin picked the full loop.

## Design decisions (locked)

- **The coding harness is the distiller/judge** — Lunaris maintains agendas
  and transactional apply-tools; it does NOT run its own distillation LLM.
  (Tin: "use same AI coding harness to do it".)
- **Reinforcement signals**: injection = weak ref; citation = strong ref
  (mechanical: Stop-hook diffs injected snippets vs turn transcript, no LLM);
  explicit `memory.feedback` MCP tool = strong ±; LLM judge runs BATCHED
  inside the dream pass, never per-turn.
- **Stale policy**: confidence decay + "⚠ code changed since" inject banner;
  invalidation only via an executed re-verify. No hard-exclude, no
  auto-invalidate in v1.
- **Dream triggers (all four, staged)**: v1 = manual `/dream` skill +
  Stop-hook threshold nudge ("N clusters await distillation") injected at
  next SessionStart; v2 options = cron autonomous session + session-end
  piggyback. All configurable.
- **Sequencing**: sdk-llamacpp-feature-forwarding task ships first, then this
  milestone, then contextd embedded-moon.
- **Credit grain (amended 2026-07-17, interview x3)**: v1 reinforcement adds a
  tool-call-level outcome signal on top of turn-level citation — post_tool
  captures already carry success/exit info; the citation detector stamps
  injected-memory ids onto the turn's tool calls and upgrades to strong+ only
  when the fed tool call succeeded. Ledger schema carries a per-signal
  `grain` field (turn|tool_call|node) from day one.
- **ATG / procedural memory (decided 2026-07-17, arXiv 2607.01942)**: Lunaris
  is the **memory substrate only** for task-graph planners — store
  decompositions + node outcomes via ingest_structured, distill successful
  plans in the dream pass. NO planner/executor inside Lunaris. Lands as a
  follow-on `procedural-memory` milestone AFTER this one (plan records are
  worthless without activation + outcome grading); the only v1 touch is
  keeping task 8's distill-kind enum extensible for a future `procedure` kind.

## Grounding (verified in code 2026-07-16 — do not re-derive)

- Phase 14 reflect loop is COMPLETE but has ZERO production callers:
  `ReflectSupervisor` (lunaris-verify/reflect.rs) → `{invalidate, boost,
  pre_warm_query}`; `reflect_apply` = atomic MVCC tombstone (closes
  valid_to) + audit; boost = `RetrievalBuilder::with_boost_cache` post-
  hydrate re-rank (in-memory LRU — ephemeral); entry `ScopedLunaris::
  end_turn`, never called by hook/mcp/server.
- ACT-R consolidator (lunaris-consolidate, default-OFF): Anderson-1996
  activation + Petrov O(1) running sum + promote/archive thresholds +
  Leiden communities. Sole reference source today = ingest events
  (`__lunaris_consolidate__` published from lunaris/src/ingest.rs) —
  activation currently measures WRITE frequency, not usefulness.
- Feedback plumbing exists, inert: `trace_injection` writes
  `lunaris:memory_injection` (injection_id + memory_ids);
  `capture_feedback` writes `lunaris:turn_feedback` (injected_memory_ids +
  outcome). Nothing consumes them.
- **BUG found**: `excluded_context_source` (context.rs:1300) filters only
  `lunaris:`-prefixed feedback/injection/session sources; the codex adapter
  writes `codex:turn_feedback` etc. → feedback records leak into prompt
  injections TODAY. Fix by matching the suffix, not the full literal.
- Typed-knowledge beachhead: `source_priority` ranks `decision:`=90,
  `edit:`=85; session_start digest = `decision:` only
  (`default_digest_prefixes`); `memory.record_decision` MCP tool shipped.
- Git plumbing point: `RecallAfterTool` already carries `paths`; captures
  store payload JSON but no first-class `git_head`/`files[]` metadata.
- MCP server has 11 tools; `server_boot.rs` roster test must stay green;
  every new tool response DTO must be a flat struct (rmcp outputSchema
  root-object rule).

## Task breakdown (breadth-first)

1. **feedback-exclusion-fix** (fast): suffix-match excluded sources; add
   `codex:*` leak regression test. Ships alone — live pollution bug.
2. **activation-ledger**: persistent per-memory activation sidecar
   (`lunaris:{scope}:activation:{ulid}`, Petrov running sum + last-ref
   wall). Writers: injection trace (weak), citation detector (strong),
   `memory.feedback` (strong ±). Readers: recall re-rank prior via the
   existing `with_boost_cache` seam (persistent provider replaces the LRU);
   ACT-R promote/archive worker reads the same ledger. AMENDED
   (2026-07-17): each reference entry carries a `grain` discriminator
   (`turn|tool_call|node` — `node` reserved for the procedural-memory
   follow-on) so finer credit needs no schema migration.
3. **citation-detector**: Stop-hook mechanically diffs injected snippet
   n-grams vs the turn's final assistant message (transcript_path from the
   Stop payload) → per-memory cited/uncited → TurnFeedback upgrade
   (per-memory verdicts, not just id list). AMENDED (2026-07-17): also
   emit tool-call-grain signals — attribute injected-memory ids to the
   turn's tool calls (transcript order) and grade strong+ only when the
   fed tool call succeeded (post_tool success/exit already captured);
   failed-tool-call feds stay weak. Ledger writes carry
   `grain: turn|tool_call` accordingly.
4. **memory.feedback MCP tool**: explicit ± with reason; flat DTO; roster
   test bump to 12.
5. **git-anchoring**: contextd stamps `git_head` + `files[]` metadata on
   every capture (paths already arrive on post_tool; prompt-phase captures
   stamp HEAD only). Backfill: none (new captures only).
6. **staleness-pass + verify-agenda**: SessionStart (and commit-shaped
   post_tool events) diff anchors vs HEAD → agenda entries; recall re-rank
   applies decay to stale-anchored hits; inject renders the ⚠ banner.
7. **memory.verify_agenda + memory.resolve MCP tools**: harness pulls stale
   memories with current-code diff context; resolve = keep / supersede /
   invalidate via the reflect_apply tombstone path (reuse, don't fork).
8. **dream-agenda + typed distillation tools**: Leiden-cluster raw episodes
   + activation stats → `memory.dream_agenda`; `memory.distill` writes
   typed records (kind ∈ decision|lesson|invariant|gotcha, provenance ids);
   distilled sources archived (activation drop, provenance preserved);
   digest prefixes + source_priority extended to the new kinds.
9. **/dream skill + stop-hook nudge**: skill drives agenda→distill→resolve;
   Stop hook computes agenda size, SessionStart injects the nudge line when
   over threshold. Cron + piggyback modes behind env flags (v2, doc-only
   stubs acceptable at exit).

## Exit criteria

- The codex feedback-leak is dead (regression test).
- A memory that gets cited twice demonstrably outranks an equal-similarity
  uncited memory in recall (discriminating integration test, real backend).
- Editing a file that a memory anchors to visibly decays + banners that
  memory in the next inject (E2E hook test).
- A full manual `/dream` run on a real scope produces typed records that
  subsequently appear in session_start digest and prompt injects.
- All new MCP tools in the server_boot roster; production path proven per
  built≠wired doctrine (discriminating tests on the REAL ingest/recall
  path, not just unit tests).
- LongMemEval smoke unchanged or better (no recall-quality regression from
  the re-rank prior).
