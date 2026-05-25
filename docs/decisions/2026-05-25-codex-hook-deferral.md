# ADR: Codex Hook Parity Deferral (HOOK-07 path b)

**Date:** 2026-05-25
**Status:** Accepted
**Requirement:** HOOK-07
**Outcome:** Path (b) — explicit ADR deferral

---

## Context

HOOK-07 required a decision: either deliver `lunaris-hook` support for Codex
CLI lifecycle events (path a), or document the primary-source finding and defer
(path b).

Claude Code's lifecycle hook system is formally documented at
`https://docs.anthropic.com/en/docs/claude-code/hooks` with a defined JSON
envelope schema (`hook_event_name`, `session_id`, `cwd`, `tool_name`,
`tool_input`, `tool_response`). Phase 23 implements `lunaris-hook` against
this documented schema.

The question: does Codex CLI have an equivalent documented hook system?

---

## Primary-Source Finding

Checked on 2026-05-25. URLs examined:

| URL | Status | Finding |
|-----|--------|---------|
| https://platform.openai.com/docs/codex | HTTP 403 — access denied by server | NO_HOOKS: docs inaccessible; no hook content reachable |
| https://openai.com/blog/openai-codex | HTTP 403 — access denied by server | NO_HOOKS: page inaccessible; no hook content reachable |
| https://github.com/openai/codex | HTTP 200 — README returned | NO_HOOKS: README contains no mention of hook, lifecycle, event, plugin, or extension APIs; GitHub tree search for "hook" or "lifecycle" files returned zero results |
| https://platform.openai.com/docs/codex/hooks | HTTP 403 — access denied by server | NOT_FOUND: no public page exists at this URL |

**Conclusion:** No publicly documented Codex CLI hook API equivalent to Claude
Code's lifecycle hooks was found as of 2026-05-25. The GitHub repository
README (the only URL that returned HTTP 200) contains no mention of hooks or
lifecycle events. Codex may have internal or beta hook capabilities, but they
are not primary-source-verifiable from public documentation.

---

## Decision

Defer Codex hook parity (path b). Phase 23 `lunaris-hook` targets the Claude
Code envelope only.

Phase 24's filter policy, secret scrubber, dedupe key, and latency gate
(HOOK-03/04/05/06) all assume a single-vendor envelope shape (Claude Code).
A future follow-up phase will re-run Phase 23/24 success criteria against
Codex envelopes once Codex publishes a hook API.

---

## Rationale

- **Shipping Claude-Code-only is the right call now.** Implementing Codex parity
  against an undocumented API risks silent breakage on every Codex CLI update.
  A documented, stable hook schema is a prerequisite for a supportable integration.
- **No HOOK work is blocked.** Phase 24 can proceed against the settled Claude
  Code envelope without waiting for Codex.
- **The resume condition is concrete.** When OpenAI publishes hook documentation
  (at `docs.openai.com/codex/hooks` or equivalent), file a new phase, confirm
  the schema maps to `lunaris-hook`'s HookEvent enum, add a smoke test at
  `crates/lunaris-hook/tests/codex_envelope.rs`, and update
  `docs/integration/codex.md` §Hook integration.

---

## Consequences

- `lunaris-hook` v0.5 is Claude-Code-only. Codex users must use
  `lunaris-mcp` (MCP tool calls) for memory capture until Codex ships hooks.
- `docs/integration/codex.md` §Deferred table is updated with a row pointing
  to this ADR.
- Phase 24 HOOK-03/04/05/06 success criteria assume Claude Code envelope only.
- Future Codex follow-up phase: map Codex envelope fields to `HookEvent` enum
  variants, add `codex_envelope.rs` smoke test, update `hooks.md`.

---

## Cross-References

- `docs/integration/codex.md` — Codex integration guide (see Deferred table)
- `docs/integration/claude-code.md` — Claude Code integration guide
- `docs/integration/hooks.md` — Hook integration guide (v0.5 Wave B scaffold)
- `docs/decisions/2026-05-24-claude-code-mcp-reversal.md` — prior MCP ADR
- ROADMAP Phase 23 Success Criterion 4
