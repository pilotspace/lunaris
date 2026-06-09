# Claude Code Integration

Status: stdio MCP plus lifecycle hooks and context injection. A plain
`npx`/`uvx`/`cargo install` MCP install defaults to per-scope SQLite; the
`setup-lunaris-agents.py` agent setup defaults to Moon-backed storage (its
hooks need Moon's queues), with a SQLite opt-out. Eleven tools live — seven
durable-memory tools (`memory.ingest`, `memory.recall`, `memory.forget`,
`memory.list_scopes`, `memory.record_decision`, `memory.record_edit`,
`memory.status`) plus four working-memory scratchpad tools
(`memory.scratchpad_write`, `memory.scratchpad_read`, `memory.scratchpad_grep`,
`memory.scratchpad_consolidate`).

---

## Quick Start (no Rust)

No Rust toolchain required. Choose either path:

### One-command setup

From a Lunaris checkout:

```sh
# Verified local checkout setup.
scripts/setup-lunaris-agents.py --agent claude --runner local

# Also build the vendored Moon server with its default feature set
# (mq + graph + text-index default) for moon://127.0.0.1:6380.
scripts/setup-lunaris-agents.py --agent claude --runner local --build-moon

# Packaged MCP runner modes, once the PyPI/npm packages are published.
scripts/setup-lunaris-agents.py --agent claude --runner uvx
scripts/setup-lunaris-agents.py --agent claude --runner npx

# Preview without writing ~/.claude/settings.json.
scripts/setup-lunaris-agents.py --agent claude --runner local --dry-run
```

The script writes `mcpServers.lunaris`, installs the same Lunaris feature set
used by Codex where Claude Code supports it, and creates
`~/.claude/settings.json.bak` before writing. Use `--hooks off` for MCP-only
setup. Add `--build-moon` to compile the vendored Moon release binary. Moon
default feature set enables `mq`, graph, and text-index support.

Installed Claude Code hooks:

| Event | Lunaris behavior |
|-------|------------------|
| `SessionStart` | capture session boundary |
| `UserPromptSubmit` / `UserPromptExpansion` | capture prompt text and inject recalled memory through `additionalContext` |
| `PreToolUse` | fast tool-call capture through `lunaris-contextd` |
| `PostToolUse` | fast tool-result capture and inject related memory through `additionalContext` |
| `PreCompact` / `PostCompact` | capture compaction boundary |
| `SubagentStart` / `SubagentStop` | capture subagent boundary; post-subagent recall on stop |
| `Stop` | capture boundary and write turn feedback |

Prompt and post-tool context injection use Claude Code's
`hookSpecificOutput.additionalContext` field. Capture paths use the same
`lunaris-codex-hook-adapter.py` sidecar protocol as Codex so `lunaris-contextd`
keeps model and storage handles warm. On Moon, captured tool chunks are also
published to the `__lunaris_embed__` queue and promoted to semantic vectors by a
background contextd worker, so hooks return before local embedding work runs.

By default the script points Claude Code at Moon:

```json
{
  "LUNARIS_MCP_STORAGE": "moon://127.0.0.1:6380",
  "LUNARIS_STORE_URL": "moon://127.0.0.1:6380",
  "LUNARIS_GRAPH_ENABLED": "1"
}
```

Keep Moon running at that URL, pass `--moon-url moon://host:port`, or choose
`--storage-backend sqlite` for per-scope SQLite.

### Via npm / npx

```bash
npx @pilotspace/lunaris-mcp --help
```

To register as a persistent Claude Code MCP server:

```json
{
  "mcpServers": {
    "lunaris": {
      "command": "npx",
      "args": ["-y", "@pilotspace/lunaris-mcp"]
    }
  }
}
```

Or using the Claude Code CLI:

```bash
claude mcp add --transport stdio lunaris -- npx -y @pilotspace/lunaris-mcp
```

### Via uv / uvx

```bash
uvx lunaris-mcp --help
# Or install permanently:
pip install lunaris-mcp
lunaris-mcp --help
```

To configure as a persistent Claude Code MCP server using uvx:

```json
{
  "mcpServers": {
    "lunaris": {
      "command": "uvx",
      "args": ["lunaris-mcp"]
    }
  }
}
```

Or using the Claude Code CLI:

```bash
claude mcp add --transport stdio lunaris -- uvx lunaris-mcp
```

Both paths download a prebuilt binary for your platform on first run.
Supported platforms: `linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64`, `win32-x64`.

**Air-gap / offline environments (npm path):** Set `LUNARIS_MCP_BIN_PATH=/path/to/lunaris-mcp` to bypass
the postinstall download and point directly at a pre-staged binary. For the pip/uvx path, use
`cargo install lunaris-mcp` to build from source instead.

---

## Prerequisites

| Requirement | Version |
|-------------|---------|
| Rust toolchain | 1.94+ |
| Claude Code | latest |
| Moon or Postgres (optional) | HNSW-accelerated recall for >10k-vector corpora |

`memory.recall` works on the default SQLite backend via brute-force cosine
(Wave A.1). For corpora larger than ~10k vectors per scope, switch to Moon
(HNSW) or Postgres (pgvector) for HNSW-class latency. The default SQLite
path supports ten of the eleven tools without any external process; only
`memory.scratchpad_consolidate` needs a native-queue backend (Moon or
Postgres) and returns `{ status: "unsupported_backend" }` on SQLite.

---

## Installation

```sh
cargo install lunaris-mcp
```

The binary lands at `~/.cargo/bin/lunaris-mcp`. Verify:

```sh
lunaris-mcp --help
```

---

## 5-Step Walkthrough

### Step 1 — Register the server (project scope, VCS-shared)

```sh
claude mcp add --transport stdio lunaris -- lunaris-mcp
```

This writes to the project-local config (`.mcp.json` in the repo root if you
run Claude Code from a git repo, or `~/.claude.json` under the project key
otherwise). The server name is `lunaris`.

To share the server definition with your team via version control, use the
project scope explicitly:

```sh
claude mcp add --scope project --transport stdio lunaris -- lunaris-mcp
```

This creates (or updates) `.mcp.json` at the repo root:

```json
{
  "mcpServers": {
    "lunaris": {
      "command": "lunaris-mcp",
      "args": []
    }
  }
}
```

### Step 2 — Verify the server is listed

```sh
claude mcp list
```

Expected output includes a `lunaris` entry with `stdio` transport.

### Step 3 — Start Claude Code in the repo

```sh
claude
```

The `lunaris-mcp` process starts as a child of Claude Code. Scope is derived
automatically from `git remote.origin.url` + current branch (blake3 →
`"git_<hex16>"`). If no git remote is detected, scope falls back to the
canonical cwd (blake3 → `"cwd_<hex16>"`).

### Step 4 — Ingest your first observation

Inside a Claude Code session:

```
memory.ingest  source="src:notes/architecture"  content="The ingest pipeline writes one atomic_write per episode. Adding a second call is a bug."
```

Returns:

```json
{ "lsn": "1748083200000:1" }
```

The LSN is `"{wall_ms}:{counter}"` — monotonically increasing within the scope.

### Step 5 — Recall

```
memory.recall  query="ingest pipeline atomicity"  k=3
```

Returns up to `k` hits fused from semantic (vector) + keyword (BM25) search.
Each hit includes `episode_id`, `source`, `content` (≤200 chars), `score`
(0–1), and `ingested_at` (RFC-3339).

> **SQLite note:** brute-force cosine scales comfortably to ~10k vectors per
> scope (single-developer / single-project use). For larger corpora, switch to
> Moon or Postgres via `LUNARIS_MCP_STORAGE`. See
> [Common Configurations](#common-configurations).

---

## Tool Reference

### `memory.ingest`

Store an observation into the current scope.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `source` | string | yes | Namespace / path for the episode (`src:notes/`, `helios:fs/…`, etc.) |
| `content` | string | yes | Text of the observation |
| `t_ref` | string | no | RFC-3339 reference timestamp (defaults to wall clock) |
| `metadata` | object | no | Arbitrary JSON key-value pairs |

Returns `{ "lsn": "<wall_ms>:<counter>" }`.

---

### `memory.recall`

Retrieve memories relevant to a query.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `query` | string | yes | Natural-language query |
| `k` | integer | no | Max hits to return (default 5) |
| `filters.source_prefix` | string | no | Restrict to episodes whose source starts with this prefix |
| `as_of` | string | no | RFC-3339 timestamp — snapshot the store at this point in time |

Returns `{ "hits": [ { "episode_id", "source", "content", "score", "ingested_at" }, … ] }`.

The first call stages the GGUF embedder (~150 MB) and reranker to
`~/.lunaris/models/` — expect ~30 s on a cold start. Subsequent calls are
fast. Set `LUNARIS_MCP_SKIP_STAGE=1` if models are pre-staged.

---

### `memory.forget`

Delete memories. Exactly one of `target.source_prefix` or `target.episode_id`
must be set.

| Field | Type | Description |
|-------|------|-------------|
| `target.source_prefix` | string | Delete all episodes whose source starts with this prefix (must be non-empty) |
| `target.episode_id` | string | Delete the single episode identified by this ULID |

Returns `{ "removed": <u64> }`.

---

### `memory.list_scopes`

Enumerate all known scopes.

No input parameters.

Returns `{ "scopes": [ { "name", "created_at", "source" }, … ] }` sorted by
`created_at` ascending. Reads `~/.lunaris/scopes.json` only — never scans
`.db` files.

---

## Common Configurations

### Override scope explicitly

```sh
LUNARIS_MCP_SCOPE=my-project lunaris-mcp
```

Or in `.mcp.json`:

```json
{
  "mcpServers": {
    "lunaris": {
      "command": "lunaris-mcp",
      "args": [],
      "env": {
        "LUNARIS_MCP_SCOPE": "my-project"
      }
    }
  }
}
```

### Point at Moon for semantic, hybrid, and graph recall

```json
{
  "mcpServers": {
    "lunaris": {
      "command": "lunaris-mcp",
      "args": [],
      "env": {
        "LUNARIS_MCP_STORAGE": "moon://127.0.0.1:6380",
        "LUNARIS_GRAPH_ENABLED": "1"
      }
    }
  }
}
```

Moon runs on port 6380 by default (`../moon/target/release/moon --port 6380`).
Moon provides native vector search, BM25/hybrid fusion, graph traversal,
queues, and bi-temporal reads.
(HNSW) or Postgres (pgvector). SQLite brute-force cosine handles up to ~10k
vectors per scope; above that threshold, the Moon or Postgres backend is the
right choice.

### Custom storage URL

```sh
LUNARIS_MCP_STORAGE=sqlite:////data/lunaris/workspace.db lunaris-mcp
```

### Adjust log verbosity

```sh
LUNARIS_MCP_LOG=debug lunaris-mcp
```

Logs go to **stderr only**. stdout is the MCP JSON-RPC transport; writing
anything there corrupts the framing and causes Claude Code to silently
disconnect.

### Pre-staged GGUF models

```sh
LUNARIS_MCP_SKIP_STAGE=1 lunaris-mcp
```

Bypasses the lazy stager on the first `memory.recall` call. Use this if
models are already present under `~/.lunaris/models/`.

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `LUNARIS_MCP_SCOPE` | derived from git/cwd | Force a specific scope name |
| `LUNARIS_MCP_STORAGE` | setup writes `moon://127.0.0.1:6380`; binary fallback is per-scope SQLite | Storage backend URL |
| `LUNARIS_GRAPH_ENABLED` | setup writes `1` for Moon; binary fallback is off | Enable graph extraction/write path for graph retrieval |
| `LUNARIS_EMBED_CACHE_CAPACITY` | `2048` | Exact-text embedding cache entries per MCP process; set `0` to disable |
| `LUNARIS_CONTEXT_MAX_HITS` | `5` prompt, `3` post-tool | Shared Codex/Claude Code context injection hit cap |
| `LUNARIS_CONTEXT_EMBED_CACHE_MAX` | `256` | Maximum cached query embeddings kept by `lunaris-contextd` |
| `LUNARIS_EMBED_PROMOTION_ENABLED` | `1` | Publish sidecar capture chunks to Moon MQ for async semantic vector promotion |
| `LUNARIS_EMBED_PROMOTION_WORKER` | `1` | Run one contextd embed-promotion worker per active scope |
| `LUNARIS_EMBED_BATCH_SIZE` | `16` | Max chunks embedded per promotion batch |
| `LUNARIS_EMBED_BATCH_WAIT_MS` | `25` | Queue coalescing wait before a partial promotion batch runs |
| `LUNARIS_MCP_LOG` | `info,rmcp=warn` | `tracing`-style filter directive |
| `LUNARIS_MCP_SKIP_STAGE` | unset | Set to `1` to skip GGUF staging on first recall |

---

## Scope Derivation

Scope is resolved once at server startup and cannot be changed by wire
payloads (enforced by `#[serde(deny_unknown_fields)]` on all DTOs):

1. `--scope` flag / `LUNARIS_MCP_SCOPE` env var (highest priority)
2. `git remote.origin.url` + current branch → blake3 → `"git_<hex16>"`
3. Canonical cwd → blake3 → `"cwd_<hex16>"`

The resolved scope is persisted to `~/.lunaris/scopes.json`. To rename a
scope (e.g., `"git_3f9a…"` → `"my-project"`):

1. Edit the `name` field in `~/.lunaris/scopes.json`.
2. Restart `lunaris-mcp` (Claude Code restart restarts the child process).

---

## Multi-Window Concurrency

The default SQLite backend uses WAL mode with `busy_timeout`. Two Claude Code
windows in the same repo share the same `.db` file safely — WAL allows one
writer and multiple concurrent readers without blocking.

For Moon, standard Redis connection pooling applies; concurrent writers are
naturally serialized by Moon's single-threaded command loop.

---

## Capture surfaces

`lunaris-mcp` exposes three capture tools. Use the structured aliases when you
have intent-typed data; fall back to `memory.ingest` for raw observations.

### `memory.ingest` (general)

Write any observation as an Episode.

```json
{"name": "memory.ingest", "arguments": {
  "source": "helios/task-planner",
  "content": "Decided to use SQLite for zero-dependency onboarding."
}}
```

### `memory.record_decision` (architectural decisions)

Write a structured decision episode with `source = "decision:<scope>"`.

```json
{"name": "memory.record_decision", "arguments": {
  "decision": "Use SQLite as the default Lunaris backend",
  "rationale": "Zero external dependencies for onboarding.",
  "alternatives": ["Postgres", "Moon"],
  "tags": ["arch", "storage"],
  "dedupe_key": "decision-sqlite-default-2026-05"
}}
```

Recall decisions later: `memory.recall` with query `"SQLite backend decision"`.

### `memory.record_edit` (file edits)

Write a structured edit episode with `source = "edit:<scope>"`. The `path`
field is stored in metadata — future `memory.recall` queries can filter by path.

```json
{"name": "memory.record_edit", "arguments": {
  "path": "crates/lunaris-mcp/src/tools/ingest.rs",
  "after": "pub(crate) struct IngestParams { ... }",
  "intent": "add dedupe_key field for HOOK-05 idempotency"
}}
```

### `memory.status` (backend + MQ health)

Report the bound scope, backend capability profile, and MQ-backed queue probes
for `__lunaris_verify__` and `__lunaris_consolidate__`.

```json
{"name": "memory.status", "arguments": {}}
```

The response includes `queue_native`, `graph_native`, `rerank_native`,
`native_rrf`, `max_vector_dim`, `max_scopes_recommended`, `cypher_dialect`, and
queue depth probes. On Moon storage, `queue_native: true` confirms MCP
ingest/recall health checks are using Moon's MQ command family through Lunaris
storage.

### Working memory (`memory.scratchpad_*`)

Four scratchpad tools provide transient, key-addressed working memory under a
`scratchpad/` namespace — drafts, plans, and in-progress state, kept separate
from the durable episode log:

- `memory.scratchpad_write` — `{ key, value, namespace? }` → `{ lsn }`
- `memory.scratchpad_read` — `{ key, namespace? }` → `{ found, value }`
- `memory.scratchpad_grep` — `{ pattern, namespace? }` → `{ entries[] }`
- `memory.scratchpad_consolidate` — `{ namespace? }` → `{ status, promotions, archives }`

`memory.scratchpad_consolidate` drains the scratchpad queue and promotes/archives
notes by activation. It needs a native-queue backend (Moon or Postgres) and
returns `{ status: "unsupported_backend" }` on SQLite. See the
[MCP tool reference](../book/src/mcp/index.md#tool-surface) for the full surface.

---

## Troubleshooting

**`lunaris-mcp: command not found`**
`~/.cargo/bin` is not in `PATH`. Add `export PATH="$HOME/.cargo/bin:$PATH"` to
your shell profile and restart the terminal. Then re-run `claude mcp add`.

**Claude Code connects but tools don't appear in the tool list**
The MCP initialize handshake failed. Run `lunaris-mcp` directly in a
terminal; any startup error (scope resolution failure, storage URL parse
error) is printed to stderr. Fix the error, then restart Claude Code.

**`memory.recall` returns empty hits**
No episodes have been ingested into the current scope yet. Run
`memory.ingest` first, then retry. If you are on Moon or Postgres and still
see empty hits, verify that `LUNARIS_MCP_STORAGE` points at the correct
backend URL.

**First `memory.recall` takes ~30 seconds**
The GGUF embedder (~150 MB) and reranker are being staged to
`~/.lunaris/models/` on first use. This is a one-time cost per host. Set
`LUNARIS_MCP_SKIP_STAGE=1` if models are pre-staged via another mechanism.

**stdout corruption / Claude Code silently disconnects**
Something is writing to stdout. Check shell profile files (`.bashrc`,
`.zshrc`) for `echo` or `print` statements that run at shell startup — they
will corrupt the MCP framing. The `LUNARIS_MCP_LOG` directive controls
`lunaris-mcp`'s own output; application logs always go to stderr.

**Wrong scope — episodes not appearing**
Run `memory.list_scopes` to see all known scopes and their derivation sources.
If the server resolved a different scope than expected, either set
`LUNARIS_MCP_SCOPE` explicitly or rename the scope entry in
`~/.lunaris/scopes.json` and restart.

---

## What You Just Got

With Wave A connected:

- **Persistent, scoped memory** across Claude Code sessions for the repo.
  Every `memory.ingest` write survives process restart — stored in
  `~/.lunaris/<scope>.db` (SQLite) or Moon/Postgres.
- **Bi-temporal storage** — every episode carries a valid-time and
  transaction-time. `memory.recall` with `as_of` lets you snapshot the store
  as it existed at any past timestamp.
- **Scope isolation (RFC 0001)** — per-repo memory with zero cross-project
  bleed. The scope is bound at startup; no wire field can override it.
- **`memory.forget`** — targeted deletion by source prefix or episode ID,
  with a non-empty-prefix guard to prevent accidental total-wipe.
- **Sub-25 ms recall** — achievable on Moon (HNSW) over millions of
  bi-temporal facts and on Postgres (pgvector). SQLite brute-force cosine is
  fast enough for ≤10k vectors per scope (single-developer / single-project).

---

## When to switch from SQLite to Moon / Postgres

SQLite brute-force cosine is the right default for solo and small-team use:

- **≤10k vectors per scope** — brute-force cosine is fast enough; no external
  process required.
- **>10k vectors per scope** — switch to Moon (HNSW) or Postgres (pgvector)
  for HNSW-class latency and sub-25 ms recall at scale.

To use Moon manually, set `LUNARIS_MCP_STORAGE` in your MCP config:

```json
"env": {
  "LUNARIS_MCP_STORAGE": "moon://127.0.0.1:6380",
  "LUNARIS_GRAPH_ENABLED": "1"
}
```

Moon: `../moon/target/release/moon --port 6380`. Postgres: any `postgres://`
connection string with the `pgvector` extension installed.

---

## Deferred to Wave B/C

| Feature | Status |
|---------|--------|
| SSE transport + Bearer auth | Deferred (Option B) |
| Multi-user server mode | Deferred |
| `npx`/`uvx` distribution | Package manifests implemented; publish/release required before registry install |
| `record_decision` / `record_edit` tool aliases | Implemented (v0.5 Wave C, 2026-05-25) |
| `coding_session_memory` recipe rename | Implemented (v0.5 Wave C, 2026-05-25) |

The stdio transport (Wave A) is the supported path. Option C (MCP as a
feature flag on `lunaris-server`) was evaluated and rejected — see
`docs/decisions/2026-05-24-claude-code-mcp-reversal.md`.
