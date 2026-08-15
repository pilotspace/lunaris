# Codex CLI

Connect Lunaris to the [Codex CLI](https://github.com/openai/codex) as a
stdio MCP server. The tool surface is identical to Claude Code —
[the same eleven `memory.*` tools](./index.md#tool-surface), the same wire
DTOs — only the configuration file differs.

## Configure the server

Codex reads MCP server definitions from `~/.codex/config.toml` (override the
directory with `CODEX_HOME`). Add a `[mcp_servers.lunaris]` table for whichever
runner you installed:

```toml
# cargo-installed binary
[mcp_servers.lunaris]
command = "lunaris-mcp"
args    = []
```

```toml
# no Rust toolchain (Node)
[mcp_servers.lunaris]
command = "npx"
args    = ["-y", "@pilotspace/lunaris-mcp"]
```

```toml
# no Rust toolchain (Python)
[mcp_servers.lunaris]
command = "uvx"
args    = ["lunaris-mcp"]
```

Codex starts `lunaris-mcp` as a stdio child and is ready once the MCP
`initialize` handshake completes. Validate the config with:

```sh
codex doctor
```

## Walkthrough

Start Codex in the repo (`codex`). The scope is derived from
`git remote.origin.url` + branch (or cwd if there is no git remote). Then:

```
memory.ingest  source="src:notes/architecture"  content="The ingest pipeline writes one atomic_write per episode."
```

```json
{ "lsn": "1748083200000:1" }
```

```
memory.recall  query="ingest pipeline atomicity"  k=3
```

> **`LUNARIS_MCP_STORAGE` is required (0.7.0).** There is no default store any
> more; the server refuses to boot without one. See
> [Storage](./index.md#storage).

## Override scope or point at Moon

Set environment variables under `[mcp_servers.lunaris.env]`:

```toml
[mcp_servers.lunaris]
command = "lunaris-mcp"
args    = []

[mcp_servers.lunaris.env]
LUNARIS_MCP_SCOPE     = "my-project"
LUNARIS_MCP_STORAGE   = "moon://127.0.0.1:6380"
LUNARIS_GRAPH_ENABLED = "1"
```

Run Moon with `../moon/target/release/moon --port 6380`.

## Hooks, injection & the warm sidecar (optional)

Codex supports the same automatic capture and proactive context injection as
Claude Code, built from three local binaries:

| Binary | Purpose |
|--------|---------|
| `lunaris-mcp` | MCP tools for explicit memory operations |
| `lunaris-hook` | fast async event capture into Lunaris storage |
| `lunaris-contextd` | warm sidecar keeping model + storage handles hot for low-latency recall |

The one-command setup installs the `[mcp_servers.lunaris]` table, the capture
hooks (`session_start`, `user_prompt_submit`, `pre`/`post_tool_use`,
`pre`/`post_compact`, `subagent_*`, `stop`), and synchronous injection for
`user_prompt_submit` + `post_tool_use`:

```sh
scripts/setup-lunaris-agents.py --agent codex --runner local             # MCP + hooks
scripts/setup-lunaris-agents.py --agent codex --runner local --hooks off  # MCP only
scripts/setup-lunaris-agents.py --agent codex --runner local --dry-run    # preview
```

It backs up `~/.codex/config.toml` before writing. Injected memory arrives as
a compact `<lunaris_memory_context>` block; if the sidecar is down, slow, or
returns no high-confidence hits, Codex continues normally with no injected
memory. Full hook config, modes, and measured timings are in
[`docs/integration/codex.md`](https://github.com/pilotspace/lunaris/blob/main/docs/integration/codex.md).

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `lunaris-mcp: command not found` | Add `export PATH="$HOME/.cargo/bin:$PATH"` to your shell profile, restart the terminal. |
| Tools don't appear after Codex starts | Run `lunaris-mcp` directly; any startup error prints to stderr. |
| `memory.recall` returns empty | Nothing ingested into this scope yet — run `memory.ingest` first. Otherwise check that `LUNARIS_MCP_STORAGE` names the Moon you ingested into. |
| First recall takes ~30 s | One-time GGUF staging to `~/.lunaris/models/`. Set `LUNARIS_MCP_SKIP_STAGE=1` if pre-staged. |
| Silent disconnect | A shell-profile `echo`/`print` is writing to **stdout** and corrupting MCP framing. |
| Wrong scope | Run `memory.list_scopes`; set `LUNARIS_MCP_SCOPE` in `[mcp_servers.lunaris.env]`. |
