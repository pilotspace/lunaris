# Claude Code Hooks Integration

Status: v0.5 Wave B (scaffold). `lunaris-hook` binary reads a single Claude
Code lifecycle event from stdin and writes one Episode into Lunaris memory.

See [Deferred to Phase 24](#deferred-to-phase-24) for what isn't shipped yet
(filter policy, secret scrubber, dedupe key — Phase 24 HOOK-03/04/05/06).

---

## Quick Start

Add `lunaris-hook` to your Claude Code `hooks` configuration block:

```json
{
  "hooks": {
    "PreToolUse": [
      { "command": "lunaris-hook" }
    ],
    "PostToolUse": [
      { "command": "lunaris-hook" }
    ],
    "Stop": [
      { "command": "lunaris-hook" }
    ],
    "SessionStart": [
      { "command": "lunaris-hook" }
    ]
  }
}
```

Claude Code pipes a JSON envelope to `lunaris-hook` stdin on each lifecycle
event. `lunaris-hook` writes one Episode to the same Lunaris memory store
as your `lunaris-mcp` server (same `~/.lunaris/<scope>.db`).

---

## Scope derivation

`lunaris-hook` derives the active scope the same way `lunaris-mcp` does:

1. `LUNARIS_HOOK_SCOPE` env var override (highest priority).
2. `git remote.origin.url + branch` → blake3 → `"git_<hex16>"`.
3. Canonical cwd → blake3 → `"cwd_<hex16>"`.

Scopes are persisted at `~/.lunaris/scopes.json`. Episodes written by
`lunaris-hook` land in the same scope as your MCP session, so
`memory.recall` queries surface both hook-captured and MCP-captured episodes.

---

## Supported event kinds

| Event | Episode source | Content |
|-------|---------------|---------|
| `PreToolUse` | `claude-code:pre_tool_use` | `tool_input` JSON |
| `PostToolUse` | `claude-code:post_tool_use` | `tool_input` + `tool_response` JSON |
| `Stop` | `claude-code:stop` | `"stop event"` |
| `SessionStart` | `claude-code:session_start` | `"session_start event"` |
| *(any other)* | *(no Episode written)* | exits 0 (forward-compat no-op) |

Unknown event kinds exit 0 (intentional no-op). This ensures forward
compatibility when Anthropic adds new hook event types to Claude Code.

---

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success (Episode written) **or** unknown event kind (forward-compat no-op) |
| 64 | Parse error (invalid JSON or missing `hook_event_name`) |
| 65 | Ingest error (storage rejected the write) |
| 66 | Phase 24 reserved (filter-rejected; not used in Phase 23) |
| 73 | Internal error (scope derivation, storage open) |

Structured JSON errors on stderr when `LUNARIS_HOOK_LOG_JSON=1`.

---

## Environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `LUNARIS_HOOK_SCOPE` | *(derived)* | Override scope name |
| `LUNARIS_STORE_URL` | `sqlite://~/.lunaris/<scope>.db` | Storage URL (shared with `lunaris-mcp`) |
| `LUNARIS_HOOK_LOG` | `warn` | Log filter (RUST_LOG syntax) |
| `LUNARIS_HOOK_LOG_JSON` | *(unset)* | Set to `1` for structured JSON stderr |
| `LUNARIS_SCOPES_FILE` | `~/.lunaris/scopes.json` | Scopes registry path |

---

## Deferred to Phase 24

| Feature | Phase |
|---------|-------|
| Path glob deny-list (`**/.env`, `**/*.pem`, etc.) | Phase 24 (HOOK-03) |
| Event kind filter (`Read` filtered by default) | Phase 24 (HOOK-03) |
| Content truncation (>128 KiB) | Phase 24 (HOOK-03) |
| Secret scrubber (AWS keys, GitHub tokens, JWTs) | Phase 24 (HOOK-04) |
| Blake3 dedupe key + idempotent re-ingest | Phase 24 (HOOK-05) |
| Cold-start latency gate (p50 <= 50ms) | Phase 24 (HOOK-06) |
