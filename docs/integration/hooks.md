# Claude Code Hooks Integration

`lunaris-hook` is a lightweight binary that captures Claude Code lifecycle events
into Lunaris agent memory. Each time Claude Code fires a hook (PreToolUse,
PostToolUse, Stop, or SessionStart), it pipes a JSON envelope to `lunaris-hook`
stdin. The hook filters, scrubs, dedupes, and ingests the event as one Episode —
in the same scope and storage file used by your `lunaris-mcp` server.

---

## Installation

```bash
# From crates.io (once published):
cargo install lunaris-hook

# From source:
cargo install --path crates/lunaris-hook
```

> **Future:** `npx lunaris-hook` packaging is planned for Phase 26 (no Rust
> toolchain required on developer machines).

---

## Claude Code configuration

Add `lunaris-hook` to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "", "hooks": [{ "type": "command", "command": "lunaris-hook" }] }
    ],
    "PostToolUse": [
      { "matcher": "", "hooks": [{ "type": "command", "command": "lunaris-hook" }] }
    ],
    "Stop": [
      { "matcher": "", "hooks": [{ "type": "command", "command": "lunaris-hook" }] }
    ],
    "SessionStart": [
      { "matcher": "", "hooks": [{ "type": "command", "command": "lunaris-hook" }] }
    ]
  }
}
```

Claude Code pipes a JSON envelope to `lunaris-hook` stdin on each lifecycle
event. The hook writes one Episode to `~/.lunaris/<scope>.db` — the same file
used by `lunaris-mcp` — so `memory.recall` queries surface both hook-captured
and MCP-captured episodes.

---

## Scope derivation

`lunaris-hook` derives the active scope the same way `lunaris-mcp` does:

1. `LUNARIS_HOOK_SCOPE` env var override (highest priority).
2. `git remote.origin.url + branch` → blake3 → `"git_<hex16>"`.
3. Canonical cwd → blake3 → `"cwd_<hex16>"`.

Scopes are persisted at `~/.lunaris/scopes.json`. Episodes written by
`lunaris-hook` land in the same scope as your MCP session, so `memory.recall`
queries surface both.

---

## Captured event kinds

| Event | Episode source | Captured content |
|-------|---------------|-----------------|
| `PreToolUse` | `claude-code:pre_tool_use` | `tool_input` JSON |
| `PostToolUse` | `claude-code:post_tool_use` | `tool_response` JSON |
| `Stop` | `claude-code:stop` | *(no content — boundary marker)* |
| `SessionStart` | `claude-code:session_start` | *(no content — boundary marker)* |
| *(any other)* | *(no Episode written)* | exits 0 (forward-compat no-op) |

Unknown event kinds exit 0 intentionally. This ensures forward compatibility
when Anthropic adds new hook event types to Claude Code.

---

## Filter policy (HOOK-03)

Events are filtered in three stages, applied in order:

### Stage 1: Path glob deny-list

Files matching any deny pattern are rejected before scrubbing. Built-in
deny patterns (always active, cannot be overridden):

| Pattern | Rationale |
|---------|-----------|
| `**/.env` | Environment variable files |
| `**/*.pem` | TLS certificates |
| `**/id_rsa*` | SSH private keys |
| `**/*.key` | Generic private key files |
| `**/.git/**` | Git internals |
| `**/node_modules/**` | Dependency trees (noise) |
| `**/target/**` | Rust build artifacts (noise) |

Additional excludes: set `LUNARIS_HOOK_EXCLUDE=pattern1:pattern2` (colon-separated glob patterns).
Additional includes (re-include from built-in excludes): set `LUNARIS_HOOK_INCLUDE=pattern1:pattern2`.
Note: `LUNARIS_HOOK_INCLUDE` cannot override built-in deny patterns — security posture is additive only.

### Stage 2: Event kind filter

By default, all four known event kinds are captured. To restrict:

```bash
# Only capture PreToolUse and PostToolUse:
export LUNARIS_HOOK_KINDS="PreToolUse,PostToolUse"
```

### Stage 3: Content truncation

Tool payloads larger than 128 KiB are truncated to preserve the head
(first 64 KiB) and tail (last 32 KiB) of the content with an elision marker:

```
<head:65536 bytes>
[... elided 12345 bytes ...]
<tail:32768 bytes>
```

The `truncated_bytes` count is recorded in the Episode metadata. Truncation
runs before scrubbing so secrets in the elided middle are never stored.

---

## Secret scrubber (HOOK-04)

All five built-in scrubber patterns run on every captured event's content.
They cannot be disabled — the scrubber set is closed and auditable.

| Kind | Pattern | Replacement |
|------|---------|-------------|
| `ENV_KEY` | Lines matching `^[A-Z_]+=.*` in `.env`-style content | `<REDACTED:ENV_KEY>` |
| `AWS_KEY` | `AKIA[0-9A-Z]{16}` | `<REDACTED:AWS_KEY>` |
| `GH_TOKEN` | `gh[pos]_[A-Za-z0-9]{36,}` | `<REDACTED:GH_TOKEN>` |
| `SSH_KEY` | `-----BEGIN .* PRIVATE KEY-----` blocks | `<REDACTED:SSH_KEY>` |
| `JWT` | `eyJ[A-Za-z0-9_-]{4,}\.eyJ[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}` | `<REDACTED:JWT>` |

> **False-positive risk:** The ENV_KEY pattern can match shell variables in
> Bash tool output. If you frequently capture bash commands that export
> variables, review the scrubber output. You can add a custom pattern to handle
> false positives via the TOML policy file (see below).

---

## Custom policy file (HOOK-04)

Create `~/.lunaris/hook-policy.toml` to extend the built-in policy. Extensions
are **additive only** — built-in deny patterns and scrubbers always run.

```toml
# ~/.lunaris/hook-policy.toml
# Schema: v0 (Lunaris 0.5)

[scrubbers.custom]
# Add patterns to redact beyond the five built-ins.
patterns = [
  { name = "internal_secret", pattern = "INT_SECRET_[A-Z0-9]{32}", redact_as = "<REDACTED:INTERNAL>" },
  { name = "api_key", pattern = "sk-[A-Za-z0-9]{40,}", redact_as = "<REDACTED:API_KEY>" },
]

[filters.paths]
# Extend the built-in deny list with additional glob patterns.
extra_excludes = ["**/secrets/**", "**/*.priv", "**/credentials.json"]
# Explicit re-includes: override extra_excludes (NOT built-in denies).
extra_includes = []
```

**Semantics:**
- If `~/.lunaris/hook-policy.toml` is absent → only built-ins apply. No error.
- If the TOML file is present but has a parse error → warn to stderr, fall back
  to built-ins only, exit 0. Never silently fail.
- Custom scrubber patterns are compiled once at process start and cached. The
  compilation cost is paid once per `lunaris-hook` invocation (each Claude Code
  event is a separate process).

---

## Dedupe key derivation (HOOK-05)

`lunaris-hook` computes a `blake3` dedupe key for every episode before writing:

```
dedupe_key = blake3_hex64(
    scope_bytes || 0x1F || event_id_bytes || 0x1F || canonical_json_bytes
)
```

Where:
- `scope` = the resolved scope string (e.g. `"git_a1b2c3d4e5f60001"`).
- `event_id` = the envelope's `event_id` field, or a synthetic id derived as
  `blake3_hex16(session_id || hook_event_name || transcript_path || timestamp)`
  if the field is absent.
- `canonical_json` = the **post-scrub** envelope serialized with sorted keys
  and no whitespace (deterministic byte sequence).

**Why post-scrub?** Two replays of the same event that both redact an AWS key
to `<REDACTED:AWS_KEY>` produce the same canonical JSON, and thus the same
dedupe key. Replay-safe deduplication holds even when the original secret text
differs between calls.

The dedupe key is stored in the `lunaris_dedupe` table. A second call with the
same key returns `IngestKind::Duplicate(prior_lsn)` — no second Episode is
written. The `was_duplicate` field in the MCP `memory.ingest` response reflects
this.

---

## Latency budget and emergency-drop (HOOK-06)

### Budget

The full hook pipeline (filter + scrub + dedupe + SQLite ingest) must complete
in:

| Metric | Budget |
|--------|--------|
| p50 | ≤ 50ms |
| p99 | ≤ 150ms |

These budgets are enforced by `crates/lunaris-hook/tests/cold_start.rs` over
1000 deterministic envelopes. **No GGUF model weights are loaded** at capture
time — embedding is deferred to first recall (lazy GGUF stager pattern).

Measured on M-series macOS (debug build, 2026-05-25):
- p50 = 4.7ms, p99 = 5.3ms — well within budget.

### Emergency-drop

To guarantee the hook never blocks a Claude Code tool invocation indefinitely
under storage-stall conditions, the ingest call is wrapped in:

```
tokio::time::timeout(LUNARIS_HOOK_DROP_AFTER_MS)
```

Default timeout: **100ms**. Override via:

```bash
export LUNARIS_HOOK_DROP_AFTER_MS=200   # relax for slow network storage
export LUNARIS_HOOK_DROP_AFTER_MS=50    # tighten for low-latency environments
```

Valid range: 10–10000ms. Values outside this range are clamped.

**On timeout:** The hook emits a single-line JSON warning to stderr and exits 0:

```json
{"level":"warn","event":"emergency_drop","reason":"ingest_timeout_100ms","scope":"git_a1b2...","kind":"PreToolUse"}
```

> **Security note (T-24-04-01):** The `emergency_drop` line contains the scope
> name and event kind. Operators who route `lunaris-hook` stderr to external
> log aggregators (Datadog, CloudWatch, etc.) **MUST sanitize** this output
> before forwarding — the scope name may reveal project identity.

---

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success (Episode written), OR unknown event kind (forward-compat no-op), OR filter-rejected (event denied by policy), OR emergency-drop (ingest stalled beyond timeout) |
| 64 | Parse error — invalid JSON or missing `hook_event_name` field |
| 65 | Ingest error — storage rejected the write within the timeout window |
| 73 | Internal error — scope derivation failure or storage URL error |

Exit 0 covers both success and "safe failure" cases (filter, emergency-drop).
Claude Code interprets any non-zero exit as a hook failure and may suppress
future invocations for the session.

---

## Environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `LUNARIS_HOOK_SCOPE` | *(derived)* | Override scope name directly |
| `LUNARIS_STORE_URL` | `sqlite://~/.lunaris/<scope>.db` | Storage URL (shared with `lunaris-mcp`) |
| `LUNARIS_HOOK_LOG` | `warn` | Log filter directive (RUST_LOG syntax) |
| `LUNARIS_HOOK_LOG_JSON` | *(unset)* | Set to `1` for structured JSON stderr on errors |
| `LUNARIS_SCOPES_FILE` | `~/.lunaris/scopes.json` | Scopes registry path |
| `LUNARIS_HOOK_DROP_AFTER_MS` | `100` | Emergency-drop timeout in ms (clamped 10–10000) |
| `LUNARIS_HOOK_EXCLUDE` | *(unset)* | Colon-separated extra path glob excludes |
| `LUNARIS_HOOK_INCLUDE` | *(unset)* | Colon-separated extra path glob re-includes |
| `LUNARIS_HOOK_KINDS` | *(all)* | Comma-separated event kinds to capture |
| `LUNARIS_EMBEDDER_DIR` | *(system cache)* | Override model weights directory |
| `LUNARIS_RERANKER_DIR` | *(system cache)* | Override reranker weights directory |

---

## Troubleshooting / FAQ

### The hook is writing episodes I don't expect

Check the filter policy: run with `LUNARIS_HOOK_LOG=debug` to see per-event
filter decisions logged to stderr.

```bash
LUNARIS_HOOK_LOG=debug lunaris-hook < /path/to/envelope.json
```

### I see `emergency_drop` in stderr

Storage is stalling beyond the configured timeout. Diagnose:

```bash
# Count recent drops in a log file
grep '"emergency_drop"' ~/.lunaris/hook-stderr.log | wc -l

# Check which scopes/kinds are dropping
grep '"emergency_drop"' ~/.lunaris/hook-stderr.log | jq -r '.scope + " " + .kind' | sort | uniq -c
```

If drops are frequent, check whether `~/.lunaris/<scope>.db` is on a slow
or remote filesystem. Consider increasing `LUNARIS_HOOK_DROP_AFTER_MS` or
moving the storage file to a local SSD.

### Dedupe: how do I know if an episode was a duplicate?

The `memory.ingest` MCP tool response includes `"was_duplicate": true` when
the hook's dedupe key matched a prior ingest. You can also check the
`lunaris_dedupe` table directly:

```sql
SELECT COUNT(*) FROM lunaris_dedupe WHERE scope = 'your-scope';
```

### The latency gate fails on CI

The `cold_start` test gate is designed for developer-machine hardware. On slow
CI runners the gate may breach the p99 budget. If this happens consistently:

1. Mark the test `#[ignore]` with a comment pointing to the evidence file.
2. Run the gate manually on a conforming developer machine.
3. Record the results in `milestones/v0.5-HOOK-LATENCY.json` as evidence.

The gate is not expected to fail on any modern macOS or Linux x86-64 host — if
it does, investigate whether a regex or SQLite write is taking an unexpected path
(e.g., accidental GGUF load via a transitive dependency).

### Can I capture `Read` events?

By default, `Read` is not a Claude Code hook event kind — Claude Code only
fires hooks for `PreToolUse`, `PostToolUse`, `Stop`, and `SessionStart`. If
Anthropic adds a `Read` event kind in a future release, `lunaris-hook` will
capture it automatically (unknown kinds exit 0 today).

---

## Security considerations

- **Scrubber is additive, not configurable off.** The five built-in patterns
  always run. This is intentional: a typo in `hook-policy.toml` can never
  silently disable AWS-key or JWT redaction.
- **Built-in path deny list is not overridable.** `**/.env`, `**/id_rsa*`,
  `**/*.pem`, and `**/.git/**` are always denied. Extra excludes via
  `LUNARIS_HOOK_EXCLUDE` extend, never replace, the built-in list.
- **Emergency-drop stderr contains scope + kind.** Do not route hook stderr
  to multi-tenant log aggregators without sanitization (T-24-04-01).
- **Storage URL is set by the operator.** `LUNARIS_STORE_URL` controls where
  episodes land. In multi-user environments, ensure each user has a distinct
  scope and storage path.
