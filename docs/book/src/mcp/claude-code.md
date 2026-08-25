# Claude Code

Connect Lunaris to [Claude Code](https://claude.com/claude-code) as a
stdio MCP server. Once registered, the agent can call
[the eleven `memory.*` tools](./index.md#tool-surface) to persist and recall
scope-isolated memory across sessions.

## Register the server

Pick whichever runner you installed (see [Install](./index.md#install)):

```sh
# cargo-installed binary, shared with your team via the repo's .mcp.json
claude mcp add --scope project --transport stdio lunaris \
  -e LUNARIS_MCP_STORAGE=moon://127.0.0.1:6380 \
  -- lunaris-mcp

# no Rust toolchain (Node)
claude mcp add --transport stdio lunaris \
  -e LUNARIS_MCP_STORAGE=moon://127.0.0.1:6380 \
  -- npx -y @pilotspace/lunaris-mcp

# no Rust toolchain (Python)
claude mcp add --transport stdio lunaris \
  -e LUNARIS_MCP_STORAGE=moon://127.0.0.1:6380 \
  -- uvx lunaris-mcp
```

`--scope project` writes a VCS-shared `.mcp.json` at the repo root:

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

Verify it is listed:

```sh
claude mcp list
```

Expect a `lunaris` entry with `stdio` transport.

## Walkthrough

Start Claude Code in the repo (`claude`). The `lunaris-mcp` process starts as
a child; the scope is derived automatically from `git remote.origin.url` +
branch (or the cwd if there is no git remote). Then, inside a session:

```text
memory.ingest  source="src:notes/architecture"  content="The ingest pipeline writes one atomic_write per episode. Adding a second call is a bug."
```

```json
{ "lsn": "1748083200000:1" }
```

The LSN is `"{wall_ms}:{counter}"` — monotonically increasing within the
scope. Recall it back:

```text
memory.recall  query="ingest pipeline atomicity"  k=3
```

Each hit carries `episode_id`, `source`, `content` (≤200 chars), `score`
(0–1), and `ingested_at` (RFC-3339). Recall against Moon is hybrid — native
HNSW vector search fused with BM25 keyword search.

> **`LUNARIS_MCP_STORAGE` has no default (0.7.0).** Set it, or run
> `lunaris-contextd` and let the server adopt the store contextd advertises
> (liveness-probed); with neither, it refuses to boot. See
> [Storage](./index.md#storage).

## Point at Moon

To get semantic, hybrid, and graph recall at scale, set the storage URL in
the server's `env` block:

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

Run Moon with `../moon/target/release/moon --port 6380`.

## Hooks & context injection (optional)

Beyond the explicit tools, a Lunaris checkout can install Claude Code
**lifecycle hooks** that capture prompts, tool calls, compaction, and subagent
boundaries automatically, and **inject** recalled memory through Claude Code's
`hookSpecificOutput.additionalContext` field:

```sh
scripts/setup-lunaris-agents.py --agent claude --runner local            # MCP + hooks
scripts/setup-lunaris-agents.py --agent claude --runner local --hooks off # MCP only
scripts/setup-lunaris-agents.py --agent claude --runner local --dry-run   # preview
```

The script backs up `~/.claude/settings.json` before writing. Full hook
table, the `lunaris-contextd` warm sidecar, and measured timings are in
[`docs/integration/claude-code.md`](https://github.com/pilotspace/lunaris/blob/main/docs/integration/claude-code.md).

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `lunaris-mcp: command not found` | Add `export PATH="$HOME/.cargo/bin:$PATH"` to your shell profile, restart the terminal, re-run `claude mcp add`. |
| Connected but no tools appear | The `initialize` handshake failed. Run `lunaris-mcp` directly; any startup error prints to stderr. |
| `memory.recall` returns empty | Nothing ingested into this scope yet — run `memory.ingest` first. Otherwise check that `LUNARIS_MCP_STORAGE` names the Moon you ingested into. |
| First recall takes ~30 s | One-time GGUF staging to `~/.lunaris/models/`. Set `LUNARIS_MCP_SKIP_STAGE=1` if pre-staged. |
| Silent disconnect | Something is writing to **stdout** (often an `echo`/`print` in `.bashrc`/`.zshrc`). It corrupts MCP framing. |
| Wrong scope | Run `memory.list_scopes`; set `LUNARIS_MCP_SCOPE` or rename the entry in `~/.lunaris/scopes.json` and restart. |
