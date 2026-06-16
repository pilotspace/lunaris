# @pilotspace/lunaris-mcp

Lunaris MCP server — no Rust toolchain required.

Downloads a prebuilt native binary for your platform on `npm install` and
forwards `npx @pilotspace/lunaris-mcp` to that binary with inherited stdio.

## Quick Start

```bash
npx @pilotspace/lunaris-mcp --help
```

Or register as a persistent Claude Code MCP server:

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

Via the Claude Code CLI:

```bash
claude mcp add --transport stdio lunaris -- npx -y @pilotspace/lunaris-mcp
```

## Supported Platforms

| Platform | Architecture | Target triple |
|----------|-------------|---------------|
| Linux    | x64         | `x86_64-unknown-linux-gnu` |
| Linux    | arm64       | `aarch64-unknown-linux-gnu` |
| macOS    | x64         | `x86_64-apple-darwin` |
| macOS    | arm64 (M-series) | `aarch64-apple-darwin` |
| Windows  | x64         | `x86_64-pc-windows-msvc` |

Unsupported platforms: install the Rust toolchain and run `cargo install lunaris-mcp`.
See [docs/integration/claude-code.md](https://github.com/pilotspace/lunaris/blob/main/docs/integration/claude-code.md).

## Air-gap / Offline Environments

Set `LUNARIS_MCP_BIN_PATH` to point at a pre-staged binary. The postinstall
script and wrapper both respect this override — no download occurs.

```bash
LUNARIS_MCP_BIN_PATH=/path/to/lunaris-mcp npx @pilotspace/lunaris-mcp --help
```

**Security note:** The `LUNARIS_MCP_BIN_PATH` escape hatch is an operator
control. An attacker who controls environment variables already owns the
process; the air-gap case is a pre-existing compromise scenario, not a new
attack surface introduced by this package. See `docs/integration/claude-code.md`
for the full threat model.

## Windows Requirements

**Windows:** `tar.exe` is required. Windows 10 Build 1803+ (released April 2018)
ships it by default. Older systems must either upgrade, or download the binary
manually from [GitHub Releases](https://github.com/pilotspace/lunaris/releases)
and set `LUNARIS_MCP_BIN_PATH` before invoking `npx @pilotspace/lunaris-mcp`.

## SHA-256 Integrity Verification

The postinstall script verifies the downloaded tarball's sha256 hash against
`manifest.json` (shipped inside this npm package). The manifest values are
populated by the release CI (`mcp-prebuild.yml`) and are the trust anchor for
the downloaded binary. Postinstall aborts with a non-zero exit code if the hash
does not match — indicating a tampered or corrupted artifact.

## Full Documentation

See [docs/integration/claude-code.md](https://github.com/pilotspace/lunaris/blob/main/docs/integration/claude-code.md)
for the complete integration guide including environment variables, storage
backends, and all eleven MCP tools.

## License

Apache-2.0
