# Claude Code Integration

Status: stdio MCP plus lifecycle hooks and context injection. **Moon is
required** — as of 0.7.0 there is no other backend and no guessed default, so a
plain `npx`/`uvx`/`cargo install` install needs `LUNARIS_MCP_STORAGE` set (or a
running `lunaris-contextd` advertising its store — see
[Pointing at a Moon by hand](#pointing-at-a-moon-by-hand)) or the server
refuses to boot. `setup-lunaris-agents.py` writes it for you and
expects Moon installed via the Moon repo's curl installer.

**20 tools** are registered — nine durable-memory tools (`memory.ingest`,
`memory.recall`, `memory.forget`, `memory.list_scopes`, `memory.record_decision`,
`memory.record_edit`, `memory.feedback`, `memory.status`, `memory.remember`), four
working-memory
scratchpad tools (`memory.scratchpad_write`, `memory.scratchpad_read`,
`memory.scratchpad_grep`, `memory.scratchpad_consolidate`), and five curation
tools (`memory.verify_agenda`, `memory.resolve`, `memory.dream_agenda`,
`memory.distill`, `memory.profile`) that let an agent maintain its memory rather than only append
to it, and two retention tools (`memory.retention`, `memory.retention_enforce`)
that bound how long a scope keeps anything. Nothing sweeps on a timer:
Lunaris ships no scheduler, so every retention pass is one a caller asks for.

---

## Turnkey (two commands)

From a fresh checkout to a memory-enabled Claude Code — capture on every
lifecycle event plus cross-session recall injected into your context — in
two commands — setup installs Moon for you if it is missing:

```sh
# 0. OPTIONAL — step 1 does this for you when no Moon is found.
#    Run it yourself to control placement or pin forward:
#    MOON_VERSION=0.8.7 INSTALL_DIR=/usr/local/bin
curl -fsSL https://raw.githubusercontent.com/pilotspace/lunaris/main/scripts/install-moon.sh | sh

# 1. Install: build the hook + MCP binaries, point Claude Code at Moon,
#    write ~/.claude/settings.json (backed up to .bak first).
scripts/setup-lunaris-agents.py --agent claude --runner local

# 2. Prove it: drives the EXACT installed hook commands through a
#    session-A capture and a session-B prompt recall, autostarting Moon
#    if nothing is listening. Prints two PASS lines and exits 0.
scripts/setup-lunaris-agents.py --agent claude --verify
```

Setup resolves the Moon binary in order: explicit `--moon-bin`, `moon` on
PATH, `~/.local/bin/moon` (the installer's target), then the vendored
`vendor/moon/target/release/moon` build artifact. **With no binary found it
installs one** — it runs `scripts/install-moon.sh` itself, so step 0 is
optional rather than a prerequisite. Pass `--install-moon never` to get the
old behaviour (fail with instructions). Lunaris agent setup is Moon-only, and
`--storage-backend sqlite` is rejected.

If no release tarball exists for your platform the installer builds Moon from
source, which takes several minutes — see
[`scripts/install-moon.sh`](../../scripts/install-moon.sh) for the full
ladder.

`--verify` output on success:

```text
VERIFY PASS: capture (session verify-a wrote marker … under scope lunaris-verify)
VERIFY PASS: cross-session inject (session verify-b saw session verify-a's memory in additionalContext)
```

The proof is the production path, not a mock: session B's marker arrives
through the same `lunaris-contextd` fused hybrid recall that serves a real
`UserPromptSubmit` hook. Any failure names its stage (`settings` /
`storage` / `binaries` / `capture` / `inject` / `cleanup`) and exits
non-zero without touching your settings. Moon autostart (local `moon://`
only, data under `~/.lunaris/moon-data`) can be disabled with
`--no-moon-autostart`; verify terminates its private contextd on exit and
fails the `cleanup` stage if a daemon survives.

Cold starts are budgeted: when a hook call has to launch `lunaris-contextd`
itself, the first request extends its deadline to
`LUNARIS_CONTEXT_COLD_TIMEOUT_MS` (default `15000`) so the lazy GGUF
embedder load cannot silently swallow the first prompt's recall. Warm
requests keep the regular `LUNARIS_CONTEXT_TIMEOUT_MS` (default `300`).

## Quick Start (no Rust)

No Rust toolchain required. Choose either path:

### One-command setup

From a Lunaris checkout:

```sh
# One-time Moon install (the setup below requires a Moon binary).
curl -fsSL https://raw.githubusercontent.com/pilotspace/lunaris/main/scripts/install-moon.sh | sh

# Verified local checkout setup.
scripts/setup-lunaris-agents.py --agent claude --runner local

# Packaged MCP runner modes, once the PyPI/npm packages are published.
scripts/setup-lunaris-agents.py --agent claude --runner uvx
scripts/setup-lunaris-agents.py --agent claude --runner npx

# Preview without writing ~/.claude/settings.json.
scripts/setup-lunaris-agents.py --agent claude --runner local --dry-run
```

The script writes `mcpServers.lunaris`, installs the same Lunaris feature set
used by Codex where Claude Code supports it, and creates
`~/.claude/settings.json.bak` before writing. Use `--hooks off` for MCP-only
setup. `--build-moon` (deprecated — prefer the curl installer) still compiles
the vendored Moon release binary as a dev fallback. Moon's default feature
set enables `mq`, graph, and text-index support.

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
  "LUNARIS_MCP_STORAGE": "moon://127.0.0.1:6381",
  "LUNARIS_STORE_URL": "moon://127.0.0.1:6381",
  "LUNARIS_GRAPH_ENABLED": "1"
}
```

Keep Moon running at that URL or pass `--moon-url moon://host:port`. The
agent setup is Moon-only; `--storage-backend sqlite` is rejected with the
curl install hint.

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
claude mcp add --transport stdio lunaris \
  -e LUNARIS_MCP_STORAGE=moon://127.0.0.1:6381 \
  -- npx -y @pilotspace/lunaris-mcp
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
claude mcp add --transport stdio lunaris \
  -e LUNARIS_MCP_STORAGE=moon://127.0.0.1:6381 \
  -- uvx lunaris-mcp
```

Both paths download a prebuilt binary for your platform on first run.
Supported platforms: `linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64`, `win32-x64`.

**Air-gap / offline environments (npm path):** Set `LUNARIS_MCP_BIN_PATH=/path/to/lunaris-mcp` to bypass
the postinstall download and point directly at a pre-staged binary. For the pip/uvx path, use
`cargo install --git https://github.com/pilotspace/lunaris lunaris-mcp` to build from source
instead (the crate is not on crates.io — see Installation below).

---

## Prerequisites

| Requirement | Version |
|-------------|---------|
| Rust toolchain | 1.94+ |
| Claude Code | latest |
| Moon (**required**) | The only storage backend; start it with `--shards 1` |

`memory.recall` fuses native HNSW vector search with BM25 keyword search on
Moon, and all 20 tools are available. There is no zero-dependency
fallback: the SQLite backend was deleted in 0.7.0, so a store must come from
`LUNARIS_MCP_STORAGE` or from a running `lunaris-contextd` advertising one in
`~/.lunaris/contextd-moon.url` (liveness-probed). With neither, the server
refuses to boot.

---

## Installation

`lunaris-mcp` is **not published to crates.io**: it links
`lunaris-memory-service`, which carries a `vendor/` path dependency and is
therefore `publish = false`, and crates.io rejects a crate whose dependencies
are unpublished. Plain `cargo install lunaris-mcp` will fail. Build from the
git source instead (needs Rust 1.94, `cmake`, and a C++ compiler for
llama.cpp):

```sh
cargo install --git https://github.com/pilotspace/lunaris lunaris-mcp
```

Prefer no Rust toolchain at all? Use `npx -y @pilotspace/lunaris-mcp` or
`uvx lunaris-mcp` (above), or extract the prebuilt
`lunaris-mcp-<target>.tar.gz` from a
[GitHub release](https://github.com/pilotspace/lunaris/releases).

The `cargo install` binary lands at `~/.cargo/bin/lunaris-mcp`. Verify:

```sh
lunaris-mcp --help
```

---

## 5-Step Walkthrough

### Step 1 — Register the server (project scope, VCS-shared)

```sh
claude mcp add --transport stdio lunaris \
  -e LUNARIS_MCP_STORAGE=moon://127.0.0.1:6381 \
  -- lunaris-mcp
```

This writes to the project-local config (`.mcp.json` in the repo root if you
run Claude Code from a git repo, or `~/.claude.json` under the project key
otherwise). The server name is `lunaris`.

To share the server definition with your team via version control, use the
project scope explicitly:

```sh
claude mcp add --scope project --transport stdio lunaris \
  -e LUNARIS_MCP_STORAGE=moon://127.0.0.1:6381 \
  -- lunaris-mcp
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

> **`LUNARIS_MCP_STORAGE` has no default** and must name a Moon when set;
> unset, a live `lunaris-contextd` store is adopted instead. See
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

**Previews by default.** Omitting `dry_run` scans and reports; it deletes
nothing. A real delete requires an explicit `"dry_run": false`. (The HTTP
`POST /v1/forget` surface is the other way round — there `dry_run` defaults to
`false` — because its callers are programs, not language models.)

| Field | Type | Description |
|-------|------|-------------|
| `target.source_prefix` | string | Delete all episodes whose source starts with this prefix (must be non-empty) |
| `target.episode_id` | string | Delete the single episode identified by this ULID |
| `dry_run` | bool | **Defaults to `true`.** `true` = preview only; `false` = commit the delete |

Returns
`{ "status": "preview" | "deleted", "dry_run": <bool>, "matched": <u64>, "removed": <u64> }`.

`matched` is what a committing call would remove; `removed` is what this call
actually removed (always `0` on a preview). The two-step flow:

```jsonc
// 1. preview — nothing is deleted
{ "target": { "source_prefix": "edit:" } }
// -> { "status": "preview", "dry_run": true, "matched": 42, "removed": 0 }

// 2. commit, once the count looks right
{ "target": { "source_prefix": "edit:" }, "dry_run": false }
// -> { "status": "deleted", "dry_run": false, "matched": 42, "removed": 42 }
```

`matched` counts what the target matches in the store, not what is still
live — an episode you already forgot keeps counting. Treat it as an upper
bound: it can over-report an already-deleted episode, never hide a live one.

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
        "LUNARIS_MCP_STORAGE": "moon://127.0.0.1:6381",
        "LUNARIS_GRAPH_ENABLED": "1"
      }
    }
  }
}
```

Moon runs on port 6381 by default
(`moon --port 6381 --shards 1 --appendonly yes`). `--shards 1` is mandatory —
an ingest is one MULTI/EXEC transaction and a sharded Moon rejects it. Moon
provides native vector search, BM25/hybrid fusion, graph traversal, queues,
and search-side bi-temporal reads.
(HNSW), measured to p50 19–22 ms at 100k documents per scope.

### Custom storage URL

```sh
LUNARIS_MCP_STORAGE=moon://moon.internal:6381 lunaris-mcp
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
| `LUNARIS_MCP_STORAGE` | *(no default)* | Storage URL; setup writes `moon://127.0.0.1:6381`. Unset: falls back to a live store advertised in `~/.lunaris/contextd-moon.url`, else refuses to boot |
| `LUNARIS_MOON_DISCOVERY_TIMEOUT_MS` | `25` | Liveness-probe budget for that discovery file (`0` disables discovery) |
| `LUNARIS_GRAPH_ENABLED` | setup writes `1`; otherwise off | Enable graph extraction/write path for graph retrieval |
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

Two Claude Code windows in the same repo share one Moon safely: standard Redis
connection pooling applies, and concurrent writers are naturally serialized by
Moon's single-threaded command loop. Each ingest is a single MULTI/EXEC
transaction, so a second window can never observe a half-written episode.

---

## Capture surfaces

`lunaris-mcp` exposes three capture tools. Use the structured aliases when you
have intent-typed data; fall back to `memory.ingest` for raw observations.

### `memory.ingest` (general)

Write any observation as an Episode.

```json
{"name": "memory.ingest", "arguments": {
  "source": "helios/task-planner",
  "content": "Retry budget for the extractor call is 3 with jittered backoff."
}}
```

### `memory.record_decision` (architectural decisions)

Write a structured decision episode with `source = "decision:<scope>"`.

```json
{"name": "memory.record_decision", "arguments": {
  "decision": "Make Moon the only Lunaris storage backend",
  "rationale": "One substrate to test, tune, and operate; the portability proof cost more than it bought.",
  "alternatives": ["Keep Postgres as a portability proof", "Keep SQLite for onboarding"],
  "tags": ["arch", "storage"],
  "dedupe_key": "decision-moon-only-2026-08"
}}
```

Recall decisions later: `memory.recall` with query `"storage backend decision"`.

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
notes by activation. It needs a native-queue backend, which Moon has. See the
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
`memory.ingest` first, then retry. If hits are still empty, verify that
`LUNARIS_MCP_STORAGE` names the same Moon you ingested into.

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
  Every `memory.ingest` write survives process restart — stored in the Moon
  named by `LUNARIS_MCP_STORAGE` (start it with `--appendonly yes` so a Moon
  restart survives too).
- **Bi-temporal storage** — every episode carries a valid-time and
  transaction-time. `memory.recall` with `as_of` lets you snapshot the store
  as it existed at any past timestamp.
- **Scope isolation (RFC 0001)** — per-repo memory with zero cross-project
  bleed. The scope is bound at startup; no wire field can override it.
- **`memory.forget`** — targeted deletion by source prefix or episode ID,
  with a non-empty-prefix guard to prevent accidental total-wipe and a
  preview-by-default `dry_run` (an agent that forgets to think about deletion
  gets a match count, not a data loss).
- **Sub-25 ms recall** — Moon's HNSW index; measured p50 19–22 ms / p99 23.4–24.4 ms at 100k documents per scope ([capacity.md](../operations/capacity.md)). Unvalidated beyond 100k.

---

## Pointing at a Moon by hand

If you are not using `setup-lunaris-agents.py`, set `LUNARIS_MCP_STORAGE`
yourself — there is no guessed default:

```json
"env": {
  "LUNARIS_MCP_STORAGE": "moon://127.0.0.1:6381",
  "LUNARIS_GRAPH_ENABLED": "1"
}
```

Start Moon with `moon --port 6381 --shards 1 --appendonly yes`, or run the
`ghcr.io/pilotspace/moon` image with the same flags. Production setup is in
[`docs/operations/external-moon.md`](../operations/external-moon.md).

### Or: let `lunaris-contextd` supply the store

If you already run `lunaris-contextd` with its embedded Moon, you can leave
`LUNARIS_MCP_STORAGE` unset. contextd advertises its endpoint in
`~/.lunaris/contextd-moon.url`, and `lunaris-mcp` adopts it after a loopback +
RESP `PING` liveness probe (25 ms, `LUNARIS_MOON_DISCOVERY_TIMEOUT_MS`) —
the same resolution `lunaris-hook` has always used, which is what keeps the
MCP tools and the hooks in ONE store instead of two.

Caveats worth knowing:

- **Read once, at boot.** Start contextd *before* the agent. A discovery file
  that appears later is not picked up by an already-running server.
- **A stale file is declined, not trusted.** If contextd crashed and its
  ephemeral port has been recycled, the probe fails and `lunaris-mcp` refuses
  to boot (saying so) rather than writing into whatever now owns that port.
- **Explicit still wins.** Setting `LUNARIS_MCP_STORAGE` skips discovery
  entirely. If you set it to a *different* Moon than contextd is using, the
  MCP proxy will refuse to serve ops locally rather than split one op stream
  across two stores.

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
