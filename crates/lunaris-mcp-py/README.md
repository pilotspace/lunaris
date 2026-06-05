# lunaris-mcp

Lunaris MCP server — no Rust toolchain required.

Each wheel ships a prebuilt `lunaris-mcp` binary for your platform.
`uvx` resolves the correct wheel automatically; the binary is exec'd via
`python -m lunaris_mcp`.

## Quick Start

### via uvx (recommended)

```bash
uvx lunaris-mcp --help
```

### via pip

```bash
pip install lunaris-mcp
lunaris-mcp --help
```

## Configure as an MCP server

### Claude Code (`~/.claude.json`)

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

Or using npx:

```json
{
  "mcpServers": {
    "lunaris": {
      "command": "npx",
      "args": ["-y", "@lunaris/mcp"]
    }
  }
}
```

### Codex (`~/.codex/config.toml`)

```toml
[mcp_servers.lunaris]
command = "uvx"
args    = ["lunaris-mcp"]
```

## Supported Platforms

| Platform | Wheel tag |
|----------|-----------|
| Linux x86_64 | `manylinux_2_28_x86_64` |
| Linux arm64 | `manylinux_2_28_aarch64` |
| macOS x86_64 | `macosx_12_0_x86_64` |
| macOS arm64 (M1/M2/M3) | `macosx_12_0_arm64` |
| Windows x64 | `win_amd64` |

## Requirements

- Python 3.11+
- No Rust toolchain required
- `uv` (recommended) or `pip`

## If the binary is missing

If the binary is absent from the wheel (local dev, custom build), the
`lunaris-mcp` entry point will print a clear error and suggest:

```bash
cargo install lunaris-mcp
```

## Air-gap / offline

For offline environments, install the Rust toolchain and build from source:

```bash
cargo install lunaris-mcp
```

Then use the `lunaris-mcp` binary directly.

## Full integration guide

See [docs/integration/claude-code.md](https://github.com/pilotspace/lunaris/blob/main/docs/integration/claude-code.md)
and [docs/integration/codex.md](https://github.com/pilotspace/lunaris/blob/main/docs/integration/codex.md)
for complete configuration examples, environment variables, and troubleshooting.
