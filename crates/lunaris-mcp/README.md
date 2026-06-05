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

## Known limitations (Wave A)

`memory.recall` in vector mode requires a Moon or Postgres backend. The
default SQLite backend supports `memory.ingest`, `memory.forget`, and
`memory.list_scopes` fully. SQLite vector search (brute-force cosine) is a
planned fast-follow.

## License

Apache-2.0 OR MIT
