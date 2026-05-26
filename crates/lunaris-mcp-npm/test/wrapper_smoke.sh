#!/usr/bin/env bash
# Smoke test for crates/lunaris-mcp-npm/bin/wrapper.js
# Run from the project root: bash crates/lunaris-mcp-npm/test/wrapper_smoke.sh
#
# Uses /bin/echo as the binary target — universally available, no network.
# Tests the LUNARIS_MCP_BIN_PATH bypass path (no download).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PKG_DIR="$(dirname "$SCRIPT_DIR")"

echo "--- wrapper smoke test ---"

# Assert wrapper.js exists (RED: this assertion will fail before Task 2)
if [[ ! -f "$PKG_DIR/bin/wrapper.js" ]]; then
  echo "FAIL: bin/wrapper.js not found — run Task 2 (GREEN) first" >&2
  exit 1
fi

# Run with LUNARIS_MCP_BIN_PATH pointing at /bin/echo (no download needed)
LUNARIS_MCP_BIN_PATH=/bin/echo node "$PKG_DIR/bin/wrapper.js" --help
echo "PASS: wrapper.js spawned the binary and exited 0"
