# lunaris-mcp

MCP server for [Lunaris](https://github.com/pilotspace/lunaris) — the
production-grade agent memory engine.

`lunaris-mcp` exposes Lunaris memory to Claude Code, OpenAI Codex, and any
other MCP-native agent via the stdio transport.

**16 tools** are registered — eight durable-memory tools (`memory.ingest`,
`memory.recall`, `memory.forget`, `memory.list_scopes`, `memory.record_decision`,
`memory.record_edit`, `memory.feedback`, `memory.status`), four working-memory
scratchpad tools (`memory.scratchpad_write`, `memory.scratchpad_read`,
`memory.scratchpad_grep`, `memory.scratchpad_consolidate`), and four curation
tools (`memory.verify_agenda`, `memory.resolve`, `memory.dream_agenda`,
`memory.distill`) that let an agent maintain its memory rather than only append
to it.

## Install

`lunaris-mcp` is **not published to crates.io**: it links
`lunaris-memory-service`, which carries a `vendor/` path dependency and is
therefore `publish = false`, and crates.io rejects a crate whose
dependencies are unpublished. Use one of the three real channels instead.

```sh
# 1. npx — prebuilt binary, no Rust toolchain
npx -y @pilotspace/lunaris-mcp

# 2. uvx — same binary, wrapped in a Python wheel
uvx lunaris-mcp

# 3. from source (needs Rust 1.94, cmake, and a C++ compiler for llama.cpp)
cargo install --git https://github.com/pilotspace/lunaris lunaris-mcp
```

From 0.6.1 onward, prebuilt `lunaris-mcp-<target>.tar.gz` tarballs are
also attached to each
[GitHub release](https://github.com/pilotspace/lunaris/releases) —
extract one and put the binary on your `PATH` if you would rather not run
a package manager at all.

## Quick start

### Claude Code

```sh
claude mcp add --scope project --transport stdio lunaris \
  -e LUNARIS_MCP_STORAGE=moon://127.0.0.1:6380 \
  -- lunaris-mcp
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

## Storage

Moon is the only backend (0.7.0 deleted SQLite and Postgres). There is **no
guessed default**; the server resolves a store in three steps:

1. `--storage` / `LUNARIS_MCP_STORAGE` — explicit always wins.
2. The store a running `lunaris-contextd` **advertises** in
   `~/.lunaris/contextd-moon.url`, adopted only after a loopback + RESP `PING`
   liveness probe (25 ms, `LUNARIS_MOON_DISCOVERY_TIMEOUT_MS`). This is how an
   MCP server and the `lunaris-hook` daemon on one machine land in the same
   Moon without being configured twice. A stale file left by a crashed
   contextd fails the probe and is declined — it never re-points you at
   whatever now owns that port. Read once, at boot: start contextd first.
3. Otherwise **refuse to boot**, printing the external-Moon quickstart. A
   stdio server shows its client tool errors, not startup logs, so "starts,
   then fails every call" is the worst outcome available.

```bash
docker run -d --name lunaris-moon -p 6380:6379 \
  ghcr.io/pilotspace/moon:0.8.5 \
  --shards 1 --protected-mode no --appendonly yes
```

`--shards 1` is mandatory — a Lunaris ingest is one MULTI/EXEC transaction and
a sharded Moon rejects it.

A source build with `--features embedded-moon` makes `lunaris-mcp`
auto-launch an in-process Moon when no `LUNARIS_MCP_STORAGE` override is set
(discovery does not run on that build — it already owns a store). The feature
is off by default and is not compiled into the published
`npx`/`uvx`/`cargo install` binaries.

## License

Apache-2.0
