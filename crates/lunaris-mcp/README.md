# lunaris-mcp

MCP server for [Lunaris](https://github.com/pilotspace/lunaris) — the
production-grade agent memory engine.

`lunaris-mcp` exposes Lunaris memory to Claude Code, OpenAI Codex, and any
other MCP-native agent via the stdio transport. Eleven tools are registered —
seven durable-memory tools (`memory.ingest`, `memory.recall`, `memory.forget`,
`memory.list_scopes`, `memory.record_decision`, `memory.record_edit`,
`memory.status`) plus four working-memory scratchpad tools
(`memory.scratchpad_write`, `memory.scratchpad_read`, `memory.scratchpad_grep`,
`memory.scratchpad_consolidate`).

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

The default SQLite backend supports ten of the eleven tools, including
`memory.recall`: it runs **vector-only** brute-force cosine (implemented in
`lunaris-storage-embedded`). The one exception is
`memory.scratchpad_consolidate`, which needs a native-queue backend (Moon or
Postgres) and returns `{ status: "unsupported_backend" }` on SQLite. BM25
keyword fusion and hybrid recall also require a keyword-capable backend —
point `LUNARIS_MCP_STORAGE` at Moon (`moon://127.0.0.1:6380`) or Postgres,
which also gives HNSW-class latency above ~10k vectors per scope.

A source build with `--features embedded-moon` makes `lunaris-mcp`
auto-launch an in-process Moon when no `LUNARIS_MCP_STORAGE` override is set.
The feature is off by default and is not compiled into the published
`npx`/`uvx`/`cargo install` binaries, so the shipped default stays SQLite.

## License

Apache-2.0
