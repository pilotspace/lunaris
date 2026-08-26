# Codex CLI Integration

Status: stdio MCP plus Codex hooks. Lunaris can be used in Codex as:

- an MCP memory server — eleven `memory.*` tools: seven durable-memory tools
  (`memory.ingest`, `memory.recall`, `memory.forget`, `memory.list_scopes`,
  `memory.record_decision`, `memory.record_edit`, `memory.status`) plus four
  working-memory scratchpad tools (`memory.scratchpad_write`,
  `memory.scratchpad_read`, `memory.scratchpad_grep`,
  `memory.scratchpad_consolidate`);
- an async capture hook for prompts, tool calls, compaction, session starts,
  subagents, and stops;
- a proactive context injector before user prompts and after useful tool calls;
- a feedback loop that records which memories were injected for later
  reflection, boost, invalidation, and pre-warm.

Parity guide to [`docs/integration/claude-code.md`](claude-code.md) — the
Claude Code guide covers all design decisions in depth; this document records
the Codex-specific differences.

---

## Quick Start (MCP only, no Rust)

No Rust toolchain required. Choose either path:

### Via npm / npx

```bash
npx @pilotspace/lunaris-mcp --help
```

To register as a persistent Codex MCP server, add to `~/.codex/config.toml`:

```toml
[mcp_servers.lunaris]
command = "npx"
args    = ["-y", "@pilotspace/lunaris-mcp"]
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

Both paths download a prebuilt `lunaris-mcp` binary for your platform on first run.
Supported platforms: `linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64`, `win32-x64`.

**Air-gap / offline environments:** Set `LUNARIS_MCP_BIN_PATH=/path/to/lunaris-mcp` to bypass
the postinstall download and point directly at a pre-staged binary.

---

## Prerequisites

| Requirement | Version |
|-------------|---------|
| Rust toolchain | 1.94+ |
| Codex CLI | latest |
| Moon (**required**) | The only storage backend; start it with `--shards 1` |

`memory.recall` fuses native HNSW vector search with BM25 keyword search on
Moon, and all 18 tools are available. There is no zero-dependency
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
`uvx lunaris-mcp`, or extract the prebuilt `lunaris-mcp-<target>.tar.gz`
from a [GitHub release](https://github.com/pilotspace/lunaris/releases).

The `cargo install` binary lands at `~/.cargo/bin/lunaris-mcp`. Verify:

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

## Full Codex Setup: MCP + Hooks + Injection

The full setup uses three local binaries:

| Binary | Purpose |
|--------|---------|
| `lunaris-mcp` | MCP tools for explicit memory operations |
| `lunaris-hook` | fast async event capture into Lunaris storage |
| `lunaris-contextd` | warm sidecar for semantic recall and Codex context injection |

### One-command setup

From a Lunaris checkout:

```sh
# Verified local checkout setup.
scripts/setup-lunaris-agents.py --agent codex --runner local

# Also build the vendored Moon server with its default feature set
# (mq + graph + text-index default) for moon://127.0.0.1:6381.
scripts/setup-lunaris-agents.py --agent codex --runner local --build-moon

# Packaged MCP runner modes, once the PyPI/npm packages are published.
scripts/setup-lunaris-agents.py --agent codex --runner uvx
scripts/setup-lunaris-agents.py --agent codex --runner npx

# Preview without writing ~/.codex/config.toml.
scripts/setup-lunaris-agents.py --agent codex --runner local --dry-run
```

The script:

- writes `[mcp_servers.lunaris]` using either the local `target/release/lunaris-mcp`
  binary or packaged runner commands like `uvx lunaris-mcp` / `npx -y @pilotspace/lunaris-mcp`;
- optionally builds the vendored Moon release binary with `--build-moon`;
  Moon's default feature set enables `mq`, graph, and text-index support;
- points storage at Moon (`moon://127.0.0.1:6381` unless `--moon-url` says
  otherwise), writing `LUNARIS_MCP_STORAGE` for MCP and `LUNARIS_STORE_URL`
  for hooks/contextd — both are **required** as of 0.7.0, and neither binary
  starts without one;
- enables `LUNARIS_GRAPH_ENABLED=1` when the effective storage URL is
  `moon://...`, so graph extraction/write paths can populate Moon-native graph
  search;
- builds `lunaris-hook` and `lunaris-contextd` when hook binaries are missing;
- installs Codex capture hooks for session, prompt, tool, compaction, subagent,
  and stop events;
- installs synchronous context injection for `user_prompt_submit` and
  `post_tool_use`;
- creates a backup at `~/.codex/config.toml.bak` before writing.

Use `--hooks off` for MCP-only setup:

```sh
scripts/setup-lunaris-agents.py --agent codex --runner local --hooks off
```

Use a different Moon instance:

```sh
scripts/setup-lunaris-agents.py --agent codex --runner local --moon-url moon://192.168.1.10:6381
```

Use `--runner uvx` or `--runner npx` after the corresponding package has been
published to PyPI/npm.

Build or install all three:

```sh
cargo build --release -p lunaris-mcp -p lunaris-hook
```

Use absolute paths in `~/.codex/config.toml` when developing from source:

```toml
[mcp_servers.lunaris]
command = "/path/to/lunaris/target/release/lunaris-mcp"
args = []

[mcp_servers.lunaris.env]
LUNARIS_MCP_LOG = "info,rmcp=warn"
LUNARIS_EMBEDDER_GGUF = "/Users/you/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf"

[hooks]
session_start = [
  { matcher = "", hooks = [
    { type = "command", command = "/path/to/lunaris/scripts/lunaris-codex-hook-adapter.py --mode capture", timeout = 2, async = true, statusMessage = "Lunaris memory capture" }
  ] },
]

user_prompt_submit = [
  { matcher = "", hooks = [
    { type = "command", command = "/path/to/lunaris/scripts/lunaris-codex-hook-adapter.py --mode capture", timeout = 2, async = true, statusMessage = "Lunaris memory capture" },
    { type = "command", command = "/path/to/lunaris/scripts/lunaris-codex-hook-adapter.py --mode inject", timeout = 2, async = false, statusMessage = "Lunaris memory recall" },
  ] },
]

pre_tool_use = [
  { matcher = "", hooks = [
    { type = "command", command = "/path/to/lunaris/scripts/lunaris-codex-hook-adapter.py --mode capture", timeout = 2, async = true, statusMessage = "Lunaris memory capture" }
  ] },
]

post_tool_use = [
  { matcher = "", hooks = [
    { type = "command", command = "/path/to/lunaris/scripts/lunaris-codex-hook-adapter.py --mode capture", timeout = 2, async = true, statusMessage = "Lunaris memory capture" },
    { type = "command", command = "/path/to/lunaris/scripts/lunaris-codex-hook-adapter.py --mode post-tool", timeout = 2, async = false, statusMessage = "Lunaris post-tool memory recall" },
  ] },
]

pre_compact = [
  { matcher = "", hooks = [
    { type = "command", command = "/path/to/lunaris/scripts/lunaris-codex-hook-adapter.py --mode capture", timeout = 2, async = true, statusMessage = "Lunaris memory capture" }
  ] },
]

post_compact = [
  { matcher = "", hooks = [
    { type = "command", command = "/path/to/lunaris/scripts/lunaris-codex-hook-adapter.py --mode capture", timeout = 2, async = true, statusMessage = "Lunaris memory capture" }
  ] },
]

subagent_start = [
  { matcher = "", hooks = [
    { type = "command", command = "/path/to/lunaris/scripts/lunaris-codex-hook-adapter.py --mode capture", timeout = 2, async = true, statusMessage = "Lunaris memory capture" }
  ] },
]

subagent_stop = [
  { matcher = "", hooks = [
    { type = "command", command = "/path/to/lunaris/scripts/lunaris-codex-hook-adapter.py --mode capture", timeout = 2, async = true, statusMessage = "Lunaris memory capture" }
  ] },
]

stop = [
  { matcher = "", hooks = [
    { type = "command", command = "/path/to/lunaris/scripts/lunaris-codex-hook-adapter.py --mode capture", timeout = 2, async = true, statusMessage = "Lunaris memory capture" },
    { type = "command", command = "/path/to/lunaris/scripts/lunaris-codex-hook-adapter.py --mode feedback", timeout = 2, async = true, statusMessage = "Lunaris memory feedback" },
  ] },
]
```

Validate the Codex config:

```sh
codex doctor
```

### What the hooks do

```text
user_prompt_submit
  |-- async capture --> lunaris-hook --> ~/.lunaris/<scope>.db
  `-- sync inject  --> lunaris-contextd --> Codex context fragment

post_tool_use
  |-- async capture --> lunaris-hook
  `-- sync recall  --> lunaris-contextd --> smaller post-tool context fragment

stop
  |-- async capture  --> lunaris-hook
  `-- async feedback --> lunaris-contextd
```

The injected context is compact and marked:

```text
<lunaris_memory_context phase="prompt">
Retrieved memories for this prompt. Use only when relevant.

- [source=decision:repo score=0.84 id=01...] Relevant prior decision...
- [source=edit:repo score=0.79 id=01...] Previous implementation detail...
</lunaris_memory_context>
```

After tool calls, the block is smaller:

```text
<lunaris_memory_context phase="post_tool" tool="Read">
Tool result may relate to these memories.

- [source=lunaris:tool_call:post score=0.72 id=01...] Prior read/edit result...
</lunaris_memory_context>
```

### Why `lunaris-contextd` exists

Hook commands are short-lived subprocesses. Loading or verifying a GGUF model
inside every hook call is too expensive. `lunaris-contextd` keeps the Lunaris
handle and model resources warm behind a local Unix socket:

```text
Codex hook process
    |
    | JSON request over ~/.lunaris/codex-contextd.sock
    v
lunaris-contextd
    |
    | scoped semantic recall
    v
~/.lunaris/<scope>.db
```

The adapter autostarts `lunaris-contextd` by default. If the sidecar is down,
slow, or returns no high-confidence hits, Codex continues normally with no
injected memory.

### Hook modes

| Mode | Event usage | Behavior |
|------|-------------|----------|
| `capture` | most events | normalize Codex envelope and forward to `lunaris-hook` |
| `inject` | `user_prompt_submit` | recall prompt-relevant memories and emit Codex `context` output |
| `post-tool` | `post_tool_use` | capture a compact tool result through the sidecar, recall related memories, emit `context` |
| `feedback` | `stop` | record turn feedback and injected memory ids when available |
| `auto` | testing/manual | capture plus injection based on event kind |

The adapter emits Codex hook context on stdout as:

```json
[{"kind":"context","text":"<lunaris_memory_context phase=\"prompt\">...</lunaris_memory_context>"}]
```

All diagnostics go to stderr.

### Tool-call memory

Codex hooks capture tool activity automatically:

| Tool phase | Source |
|------------|--------|
| pre tool call | `lunaris:pre_tool_use` through compatibility envelope |
| post tool call | `lunaris:post_tool_use` through compatibility envelope |
| sidecar tool capture | `lunaris:tool_call:pre` / `lunaris:tool_call:post` |
| injection trace | `lunaris:memory_injection` |
| turn feedback | `lunaris:turn_feedback` |

`Read`, `Edit`, `MultiEdit`, `Write`, and `Bash` are captured by default.
When `LUNARIS_CONTEXT_CAPTURE_FAST=1`, Codex pre/post tool capture is sent to
the warm sidecar and the sidecar acknowledges before doing any full semantic
recall work. Capture writes use the same embed-free `NoopEmbedder` path as
`lunaris-hook`, so tool capture does not run GGUF inference; the content remains
available to Moon's keyword/BM25 surface immediately. On Moon, contextd also
publishes a `__lunaris_embed__` MQ event and a background worker batches real
vector upserts, so semantic recall catches up without blocking the tool hook.
Secret-bearing paths such as `.env`, PEM files, SSH keys, and `.git/**` remain
denied by the built-in policy.

### Performance defaults

| Setting | Default | Purpose |
|---------|---------|---------|
| `LUNARIS_CONTEXT_CAPTURE_FAST` | `1` | route Codex pre/post tool capture through warm `lunaris-contextd` |
| `LUNARIS_CONTEXT_CAPTURE_TIMEOUT_MS` | `120` | best-effort sidecar capture wait budget |
| `LUNARIS_CONTEXT_TIMEOUT_MS` | `300` | prompt injection sidecar wait budget |
| `LUNARIS_CONTEXT_POST_TOOL_TIMEOUT_MS` | `300` | post-tool injection sidecar wait budget |
| `LUNARIS_CONTEXT_MAX_HITS` | `5` prompt, `3` post-tool | shared memory hit cap |
| `LUNARIS_CONTEXT_MAX_CHARS` | `1600` prompt, `900` post-tool | shared context size cap |
| `LUNARIS_CONTEXT_MIN_SCORE` | `0.55` prompt, `0.60` post-tool | shared score threshold |
| `LUNARIS_CONTEXT_POST_TOOL_MAX_HITS` | `3` | optional post-tool hit cap override |
| `LUNARIS_CONTEXT_POST_TOOL_MAX_CHARS` | `900` | optional post-tool context size cap override |
| `LUNARIS_CONTEXT_POST_TOOL_MIN_SCORE` | `0.60` | optional post-tool score threshold override |
| `LUNARIS_CONTEXT_EMBED_CACHE_MAX` | `256` | maximum cached query embeddings kept by `lunaris-contextd` |
| `LUNARIS_EMBED_PROMOTION_ENABLED` | `1` | publish contextd capture chunks to Moon MQ for async semantic vector promotion |
| `LUNARIS_EMBED_PROMOTION_WORKER` | `1` | run one background embed-promotion worker per active scope |
| `LUNARIS_EMBED_BATCH_SIZE` | `16` | max chunks embedded per promotion batch |
| `LUNARIS_EMBED_BATCH_WAIT_MS` | `25` | queue coalescing wait before a partial promotion batch runs |

The adapter still accepts the older `LUNARIS_CODEX_CONTEXT_*` and
`LUNARIS_CODEX_POST_TOOL_*` variables as compatibility aliases. Prefer the
shared `LUNARIS_CONTEXT_*` names for both Codex and Claude Code.

Measured local smoke on Apple Silicon with release binaries, Moon storage, and
the quantized Granite embedder:

| Path | Typical hot timing |
|------|--------------------|
| standalone `lunaris-hook` capture | p50 5.09 ms, p99 29.51 ms |
| `lunaris-contextd` capture write + MQ promotion publish | p50 0.37 ms, p99 5.72 ms |
| MCP `memory.status` Moon/MQ probe | 1.1 ms |
| MCP cached/repeated `memory.ingest` storage path | p50 0.6 ms |
| MCP unique `memory.ingest` with fresh GGUF embedding | p50 1007 ms |
| MCP hot `memory.recall` after cache warmup | p50 4.3 ms |

The first prompt for a new query can still take a few hundred milliseconds
because the local query embedding is computed once before being cached. Keep the
timeout near
300 ms unless you intentionally prefer dropping context over waiting.

### Disable or customize hooks

```sh
# Disable all Codex context injection, keep capture path usable.
export LUNARIS_CONTEXT_ENABLED=0

# Prevent adapter autostart of the sidecar.
export LUNARIS_CONTEXTD_AUTOSTART=0

# Use a custom socket.
export LUNARIS_CONTEXTD_SOCKET="$HOME/.lunaris/my-contextd.sock"

# Force storage shared by MCP and hooks.
export LUNARIS_STORE_URL="moon://127.0.0.1:6381"
export LUNARIS_GRAPH_ENABLED=1
```

---

## 5-Step Walkthrough

### Step 1 — Install and configure

```sh
# not on crates.io (publish = false dependency) — build from git:
cargo install --git https://github.com/pilotspace/lunaris lunaris-mcp
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

> **`LUNARIS_MCP_STORAGE` has no default** and must name a Moon when set.
> Unset, the server adopts the store a live `lunaris-contextd` advertises in
> `~/.lunaris/contextd-moon.url`; with neither it refuses to boot. See
> [Common Configurations](#common-configurations).

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

### Point at Moon for semantic, hybrid, and graph recall

```toml
[mcp_servers.lunaris]
command = "lunaris-mcp"
args    = []

[mcp_servers.lunaris.env]
LUNARIS_MCP_STORAGE = "moon://127.0.0.1:6381"
LUNARIS_GRAPH_ENABLED = "1"
```

Moon runs on port 6381 by default
(`moon --port 6381 --shards 1 --appendonly yes`). `--shards 1` is mandatory —
an ingest is one MULTI/EXEC transaction and a sharded Moon rejects it. Moon
provides native vector search, BM25/hybrid fusion, graph traversal, queues,
and search-side bi-temporal reads.

### A different Moon

```toml
[mcp_servers.lunaris.env]
LUNARIS_MCP_STORAGE = "moon://moon.internal:6381"
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
| `LUNARIS_MCP_STORAGE` | *(no default)* | Storage URL; setup writes `moon://127.0.0.1:6381`. Unset: falls back to a live store advertised in `~/.lunaris/contextd-moon.url`, else refuses to boot |
| `LUNARIS_MOON_DISCOVERY_TIMEOUT_MS` | `25` | Liveness-probe budget for that discovery file (`0` disables discovery) |
| `LUNARIS_GRAPH_ENABLED` | setup writes `1`; otherwise off | Enable graph extraction/write path for graph retrieval |
| `LUNARIS_EMBED_CACHE_CAPACITY` | `2048` | Exact-text embedding cache entries per MCP process; set `0` to disable |
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
| `memory.forget` | `target.source_prefix` XOR `target.episode_id`, optional `dry_run` (**defaults to `true` = preview**) | `{ status, dry_run, matched, removed }` |
| `memory.list_scopes` | _(none)_ | `{ scopes[] }` |
| `memory.record_decision` | `decision`, `rationale`, optional `alternatives`, `tags`, `dedupe_key` | `{ lsn, was_duplicate }` |
| `memory.record_edit` | `path`, `after`, optional `before`, `intent`, `dedupe_key` | `{ lsn, was_duplicate }` |
| `memory.status` | _(none)_ | backend capabilities plus MQ queue depth probes |
| `memory.scratchpad_write` | `key`, `value`, optional `namespace` | `{ lsn }` |
| `memory.scratchpad_read` | `key`, optional `namespace` | `{ found, value }` |
| `memory.scratchpad_grep` | `pattern`, optional `namespace` | `{ entries[] }` |
| `memory.scratchpad_consolidate` | optional `namespace` | `{ status, promotions, archives }` (needs a native queue; Moon has one) |

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

Same behaviour as Claude Code: two Codex windows in the same repo share one
Moon safely. Concurrent writers are serialized by Moon's single-threaded
command loop, and each ingest is a single MULTI/EXEC transaction, so no window
observes a half-written episode.

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
`memory.ingest` first, then retry. If hits are still empty, verify that
`LUNARIS_MCP_STORAGE` names the same Moon you ingested into.

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

## Pointing at a Moon by hand

`setup-lunaris-agents.py` writes the storage URL for you. If you configure
Codex yourself, set `LUNARIS_MCP_STORAGE` in `~/.codex/config.toml` — it has
no default and the server will not start without it:

```toml
[mcp_servers.lunaris.env]
LUNARIS_MCP_STORAGE = "moon://127.0.0.1:6381"
LUNARIS_GRAPH_ENABLED = "1"
```

Start Moon with `moon --port 6381 --shards 1 --appendonly yes`, or run the
`ghcr.io/pilotspace/moon` image with the same flags. Production setup is in
[`docs/operations/external-moon.md`](../operations/external-moon.md).

---

## Capture Surfaces

Lunaris has explicit MCP capture and automatic hook capture.

Use the structured MCP aliases when you have intent-typed data; fall back to
`memory.ingest` for raw observations. The Codex hooks add automatic capture for
prompt and tool lifecycle events so you do not need to manually record every
read/edit/search command.

### `memory.ingest` (general)

Write any observation as an Episode.

```json
{"name": "memory.ingest", "arguments": {
  "source": "codex/task-planner",
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

### `memory.scratchpad_*` (working memory)

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

## Deferred / Optional

| Feature | Status |
|---------|--------|
| SSE transport + Bearer auth | Deferred (Option B) |
| Multi-user server mode | Deferred |
| `npx`/`uvx` distribution | Package manifests implemented; publish/release required before registry install |
| `record_decision` / `record_edit` tool aliases | Implemented (v0.5 Wave C, 2026-05-25) |
| Codex hook capture | Implemented through `scripts/lunaris-codex-hook-adapter.py --mode capture` |
| Codex prompt context injection | Implemented through `--mode inject` + `lunaris-contextd` |
| Codex post-tool context injection | Implemented through `--mode post-tool` + `lunaris-contextd` |

The stdio transport (Wave A) is the supported path. See
`docs/decisions/2026-05-24-claude-code-mcp-reversal.md` for the Option C
rejection record.
