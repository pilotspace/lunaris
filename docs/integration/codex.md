# Codex CLI Integration

Status: Wave A (stdio + SQLite default). Four tools live: `memory.ingest`,
`memory.recall`, `memory.forget`, `memory.list_scopes`. See
[Deferred to Wave B/C](#deferred-to-wave-bc) for what isn't shipped yet.

Parity guide to [`docs/integration/claude-code.md`](claude-code.md) — the
Claude Code guide covers all design decisions in depth; this document records
the Codex-specific differences.

---

## Quick Start (no Rust)

No Rust toolchain required. Choose either path:

### Via npm / npx

```bash
npx @lunaris/mcp --help
```

To register as a persistent Codex MCP server, add to `~/.codex/config.toml`:

```toml
[mcp_servers.lunaris]
command = "npx"
args    = ["-y", "@lunaris/mcp"]
```

### Via uv / uvx (recommended for Python users)

```bash
uvx lunaris-mcp --help
# Or install permanently:
pip install lunaris-mcp
lunaris-mcp --help
```

To register as a persistent Codex MCP server using uvx, add to `~/.codex/config.toml`:

```toml
[mcp_servers.lunaris]
command = "uvx"
args    = ["lunaris-mcp"]
```

Both paths download a prebuilt binary for your platform on first run.
Supported platforms: `linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64`, `win32-x64`.

**Air-gap / offline environments:** Set `LUNARIS_MCP_BIN_PATH=/path/to/lunaris-mcp` to bypass
the postinstall download and point directly at a pre-staged binary.

---

## Prerequisites

| Requirement | Version |
|-------------|---------|
| Rust toolchain | 1.94+ |
| Codex CLI | latest |
| Moon or Postgres (optional) | HNSW-accelerated recall for >10k-vector corpora |

`memory.recall` works on the default SQLite backend via brute-force cosine
(Wave A.1). For corpora larger than ~10k vectors per scope, switch to Moon
(HNSW) or Postgres (pgvector) for HNSW-class latency. The default SQLite
path supports all four tools (`memory.ingest`, `memory.recall`,
`memory.forget`, `memory.list_scopes`) without any external process.

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

## Configuration

Codex reads MCP server definitions from `~/.codex/config.toml`. Add the
`lunaris` server under the `[mcp_servers.lunaris]` table:

```toml
[mcp_servers.lunaris]
command = "lunaris-mcp"
args    = []
```

Codex starts `lunaris-mcp` as a stdio child process. The server is ready
when Codex completes the MCP `initialize` handshake.

The `CODEX_HOME` environment variable overrides the default config directory
(`~/.codex`).

---

## 5-Step Walkthrough

### Step 1 — Install and configure

```sh
cargo install lunaris-mcp
```

Add to `~/.codex/config.toml`:

```toml
[mcp_servers.lunaris]
command = "lunaris-mcp"
args    = []
```

### Step 2 — Verify (dry-run)

```sh
lunaris-mcp --help
```

Expected: prints CLI usage with `--scope`, `--storage`, `--log-level` flags.

### Step 3 — Start Codex in the repo

```sh
codex
```

`lunaris-mcp` starts as a child process. Scope is derived from
`git remote.origin.url` + current branch (blake3 → `"git_<hex16>"`), or
cwd (blake3 → `"cwd_<hex16>"`) if no git remote is detected.

### Step 4 — Ingest your first observation

Inside a Codex session:

```
memory.ingest  source="src:notes/architecture"  content="The ingest pipeline writes one atomic_write per episode."
```

Returns:

```json
{ "lsn": "1748083200000:1" }
```

### Step 5 — Recall

```
memory.recall  query="ingest pipeline atomicity"  k=3
```

> **SQLite note:** brute-force cosine scales comfortably to ~10k vectors per
> scope (single-developer / single-project use). For larger corpora, switch to
> Moon or Postgres via `LUNARIS_MCP_STORAGE`.

---

## Common Configurations

### Override scope explicitly

```toml
[mcp_servers.lunaris]
command = "lunaris-mcp"
args    = []

[mcp_servers.lunaris.env]
LUNARIS_MCP_SCOPE = "my-project"
```

### Point at Moon for HNSW-accelerated recall

```toml
[mcp_servers.lunaris]
command = "lunaris-mcp"
args    = []

[mcp_servers.lunaris.env]
LUNARIS_MCP_STORAGE = "redis://localhost:6380"
```

Moon runs on port 6380 by default (`../moon/target/release/moon --port 6380`).
Sub-25 ms recall over millions of bi-temporal facts is achievable on Moon
(HNSW) or Postgres (pgvector). SQLite brute-force cosine handles up to ~10k
vectors per scope; above that threshold, the Moon or Postgres backend is the
right choice.

### Custom storage path

```toml
[mcp_servers.lunaris.env]
LUNARIS_MCP_STORAGE = "sqlite:////data/lunaris/workspace.db"
```

### Adjust log verbosity

```toml
[mcp_servers.lunaris.env]
LUNARIS_MCP_LOG = "debug"
```

Logs go to **stderr only**. stdout is the MCP JSON-RPC transport.

### Pre-staged GGUF models

```toml
[mcp_servers.lunaris.env]
LUNARIS_MCP_SKIP_STAGE = "1"
```

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `LUNARIS_MCP_SCOPE` | derived from git/cwd | Force a specific scope name |
| `LUNARIS_MCP_STORAGE` | `sqlite:///<HOME>/.lunaris/<scope>.db` | Storage backend URL |
| `LUNARIS_MCP_LOG` | `info,rmcp=warn` | `tracing`-style filter directive |
| `LUNARIS_MCP_SKIP_STAGE` | unset | Set to `1` to skip GGUF staging on first recall |

---

## Tool Reference

See [`docs/integration/claude-code.md#tool-reference`](claude-code.md#tool-reference)
for the full tool reference. The wire DTOs are identical across MCP clients.
Summary:

| Tool | Input | Returns |
|------|-------|---------|
| `memory.ingest` | `source`, `content`, optional `t_ref`, `metadata` | `{ lsn }` |
| `memory.recall` | `query`, optional `k`, `filters`, `as_of` | `{ hits[] }` |
| `memory.forget` | `target.source_prefix` XOR `target.episode_id` | `{ removed }` |
| `memory.list_scopes` | _(none)_ | `{ scopes[] }` |

---

## Scope Derivation

Scope is resolved once at server startup:

1. `--scope` / `LUNARIS_MCP_SCOPE` (highest priority)
2. `git remote.origin.url` + current branch → blake3 → `"git_<hex16>"`
3. Canonical cwd → blake3 → `"cwd_<hex16>"`

Persisted to `~/.lunaris/scopes.json`. To rename:

1. Edit the `name` field in `~/.lunaris/scopes.json`.
2. Restart Codex (restarts the `lunaris-mcp` child process).

---

## Multi-Window Concurrency

Same behaviour as Claude Code: the default SQLite backend runs in WAL mode
with `busy_timeout`. Two Codex windows in the same repo share the same `.db`
safely.

---

## Troubleshooting

**`lunaris-mcp: command not found`**
`~/.cargo/bin` is not in `PATH`. Add `export PATH="$HOME/.cargo/bin:$PATH"`
to your shell profile, then restart the terminal.

**Tools don't appear after Codex starts**
Run `lunaris-mcp` directly; any startup error is printed to stderr. Fix the
error, then restart Codex.

**`memory.recall` returns empty hits**
No episodes have been ingested into the current scope yet. Run
`memory.ingest` first, then retry. If you are on Moon or Postgres and still
see empty hits, verify that `LUNARIS_MCP_STORAGE` points at the correct
backend URL.

**First `memory.recall` takes ~30 seconds**
The GGUF embedder (~150 MB) and reranker are being staged to
`~/.lunaris/models/`. One-time cost per host. Set `LUNARIS_MCP_SKIP_STAGE=1`
if models are pre-staged.

**stdout corruption / Codex silently disconnects**
Shell profile files (`.bashrc`, `.zshrc`) are printing to stdout on startup.
Remove or redirect those `echo`/`print` calls — they corrupt MCP framing.

**Wrong scope**
Run `memory.list_scopes` to inspect the scope registry, then set
`LUNARIS_MCP_SCOPE` explicitly in `[mcp_servers.lunaris.env]`.

---

## When to switch from SQLite to Moon / Postgres

SQLite brute-force cosine is the right default for solo and small-team use:

- **≤10k vectors per scope** — brute-force cosine is fast enough; no external
  process required.
- **>10k vectors per scope** — switch to Moon (HNSW) or Postgres (pgvector)
  for HNSW-class latency and sub-25 ms recall at scale.

To switch, set `LUNARIS_MCP_STORAGE` in `~/.codex/config.toml`:

```toml
[mcp_servers.lunaris.env]
LUNARIS_MCP_STORAGE = "redis://localhost:6380"
```

Moon: `../moon/target/release/moon --port 6380`. Postgres: any `postgres://`
connection string with the `pgvector` extension installed.

---

## Capture surfaces

`lunaris-mcp` exposes three capture tools. Use the structured aliases when you
have intent-typed data; fall back to `memory.ingest` for raw observations.

### `memory.ingest` (general)

Write any observation as an Episode.

```json
{"name": "memory.ingest", "arguments": {
  "source": "codex/task-planner",
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

---

## Deferred to Wave B/C

| Feature | Status |
|---------|--------|
| SSE transport + Bearer auth | Deferred (Option B) |
| Multi-user server mode | Deferred |
| `npx`/`uvx` distribution | Deferred |
| `record_decision` / `record_edit` tool aliases | Implemented (v0.5 Wave C, 2026-05-25) |
| Codex hook parity (`lunaris-hook`) | Deferred — [ADR 2026-05-25](../decisions/2026-05-25-codex-hook-deferral.md) |

The stdio transport (Wave A) is the supported path. See
`docs/decisions/2026-05-24-claude-code-mcp-reversal.md` for the Option C
rejection record.
