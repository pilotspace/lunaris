# MILESTONE: Hook Session Scratchpad

goal: An agent session switch is a first-class memory event — when one coding-agent session ends and another begins, lunaris-hook consolidates the previous session's scratchpad into long-term memory (the P-C guarded path), binds a fresh per-session scratchpad, and hands the new session a distilled summary of what the last one left behind. Nothing leaks between sessions; nothing is lost; the new session starts warm.
rationale: intake bucket `sub-milestone` — confirmed by Tin Dang 2026-06-11 ("Own sub-milestone" + "Full handover" + "Yes, inject summary"). Three coupled behaviors (detect switch → handover/consolidate → inject context) over the existing v0.5 lunaris-hook adapter and the merged MCP-scratchpad machinery; each is its own freeze point.
stage: production · status: active · created: 2026-06-11

> SDD living doc for this milestone. Keep it THIN: breadth, shared decisions, and
> exit criteria only — per-task detail lives in each `.add/tasks/<slug>/TASK.md`,
> written just-in-time. Update this doc whenever a task reveals a milestone gap.

## Scope
In:  SessionEnd envelope support (today parsed as `Unknown`/ignored) · session-switch detection (hook invocations are stateless — a durable last-active-session marker is required, storage location is a frozen contract) · per-session scratchpad namespace convention shared by hook AND MCP tools · automatic guarded consolidate of the previous session's pad on switch (reuse `WorkingMemory::consolidate()` + ActR via `set_consolidator` + the P-C guards — NO new consolidation engine) · SessionStart `additionalContext` injection (distilled summary of consolidated facts + carry-over notes; build on the existing `lunaris-contextd` warm-recall sidecar where it fits)
Out: any new consolidation/extraction model or engine path · MCP transport changes (stdio stays) · Codex-fork parity beyond what HOOK-07 already established (Claude Code first; Codex follows the resolved fork) · admin/live-reload endpoints · the moon-only backend sweep (separate milestone) · multi-agent concurrent-session arbitration (two simultaneous sessions on one scope — record as constraint, defer)

## Shared decisions & glossary deltas   (living — every task must honor these)
- Session identity = the hook payload `session_id` (all four existing payloads carry it). A "switch" is observed at SessionStart whose `session_id` differs from the durable last-active marker; SessionEnd is a hint, not a requirement (crashes never emit it).
- Scratchpad namespace convention must be ONE rule shared by lunaris-hook and the MCP scratchpad tools — the per-session pad the hook rotates must be the same pad `memory.scratchpad_*` reads/writes, or the feature is theater.
- Consolidation reuses the P-C guarded path exactly (3 guards intact); auto-trigger on switch must respect the same guards as the manual MCP tool.
- Scope alphabet `[A-Za-z0-9_\-.]{1,128}` applies to any session-derived namespace component (session_ids must be sanitized before keying).
- Design for failure: a failed consolidate on switch must NEVER block the new session (log + carry the old pad forward; retry on next switch).

## Shared / risky contracts (freeze these first)
- last-active-session marker location + per-session namespace shape -> owning task session-switch-detect
- handover semantics (what happens to the old pad after consolidate: delete vs archive; failure path) -> owning task scratchpad-handover
- additionalContext payload shape + size budget -> owning task session-context-inject

## Tasks (breadth-first decomposition; detail lives in each TASK.md)
- [ ] session-switch-detect   depends-on: none                  — SessionEnd envelope variant + durable last-active-session marker + switch detection in lunaris-hook; emits a typed SwitchObserved outcome the next tasks consume
- [ ] scratchpad-handover     depends-on: session-switch-detect — on switch: guarded consolidate of the previous session's pad, then bind/rotate the per-session namespace (hook + MCP shared convention); failure carries forward, never blocks
- [ ] session-context-inject  depends-on: scratchpad-handover   — SessionStart returns additionalContext: distilled summary of the consolidated facts + carry-over notes (contextd-assisted where warm)
- [ ] consolidate-prefix-drop depends-on: none                  — SPLIT from scratchpad-handover (2026-06-11): consolidate_scoped(Some(prefix)) drops drained non-matching events (lib.rs:121-128) — silent loss for other namespaces on every namespaced memory.scratchpad_consolidate; handover sidesteps via whole-scope drains, this task owns the real fix

## Exit criteria (observable; map each to the task that delivers it)
- [ ] Killing a session and starting a new one yields a SwitchObserved with the correct old/new session_ids, even without a SessionEnd event   (← session-switch-detect)
- [ ] After the switch, the old session's scratchpad facts are queryable via memory.recall (consolidated) and the new session's scratchpad_read starts empty — proven by an integration test that drives two real sessions end-to-end   (← scratchpad-handover)
- [ ] The second session's SessionStart hook output contains the distilled summary (visible in a real Claude Code transcript or raw stdio capture)   (← session-context-inject)
- [ ] A consolidate failure injected mid-switch leaves the old pad intact and the new session functional (failure-path test)   (← scratchpad-handover)
