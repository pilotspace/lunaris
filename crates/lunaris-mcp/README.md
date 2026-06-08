# lunaris-mcp

MCP server for [Lunaris](https://github.com/pilotspace/lunaris) — the
production-grade agent memory engine.

`lunaris-mcp` exposes Lunaris memory to Claude Code, OpenAI Codex, and any
other MCP-native agent via the stdio transport. Four tools ship in Wave A:
`memory.ingest`, `memory.recall`, `memory.forget`, `memory.list_scopes`.

## Install

```sh
cargo install lunaris-mcp
```

## Quick start

### Claude Code

```sh
claude mcp add --scope project --transport stdio lunaris -- lunaris-mcp
```

### Codex

Add to `~/.codex/config.toml`:

```toml
[mcp_servers.lunaris]
command = "lunaris-mcp"
args    = []
```

## Documentation

- [Claude Code integration guide](../../docs/integration/claude-code.md)
- [Codex integration guide](../../docs/integration/codex.md)
- [Decision record: Option A (stdio) adopted](../../docs/decisions/2026-05-24-claude-code-mcp-reversal.md)

## Storage backends

The default SQLite backend supports every tool, including `memory.recall`:
it runs **vector-only** brute-force cosine (implemented in
`lunaris-storage-embedded`). BM25 keyword fusion and hybrid recall require a
keyword-capable backend — point `LUNARIS_MCP_STORAGE` at Moon
(`moon://127.0.0.1:6380`) or Postgres, which also gives HNSW-class latency
above ~10k vectors per scope.

## License

Apache-2.0 OR MIT
