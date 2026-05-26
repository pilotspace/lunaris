#!/usr/bin/env bash
# Test: postinstall MUST abort when manifest sha256 doesn't match tarball.
# Strategy: create a temp manifest with a wrong hash, write a fake tarball
# locally, point download to a file:// URL, run postinstall, assert exit non-zero.
#
# Run from the project root: bash crates/lunaris-mcp-npm/test/sha256_abort.sh
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PKG_DIR="$(dirname "$SCRIPT_DIR")"
TMPDIR_LOCAL=$(mktemp -d)
trap "rm -rf $TMPDIR_LOCAL" EXIT

echo "--- sha256 abort test ---"

# Determine expected tarball name for this platform (mirrors PLATFORM_MAP in postinstall.js)
PLAT="$(node -e "
const m={'linux-x64':'lunaris-mcp-x86_64-unknown-linux-gnu.tar.gz','linux-arm64':'lunaris-mcp-aarch64-unknown-linux-gnu.tar.gz','darwin-x64':'lunaris-mcp-x86_64-apple-darwin.tar.gz','darwin-arm64':'lunaris-mcp-aarch64-apple-darwin.tar.gz','win32-x64':'lunaris-mcp-x86_64-pc-windows-msvc.tar.gz'};
const k=process.platform+'-'+process.arch;
console.log(m[k]||'lunaris-mcp-x86_64-unknown-linux-gnu.tar.gz');
")"

# Build a fake tarball containing a tiny fake binary
echo "FAKEBINARY" > "$TMPDIR_LOCAL/lunaris-mcp"
tar czf "$TMPDIR_LOCAL/$PLAT" -C "$TMPDIR_LOCAL" lunaris-mcp
rm "$TMPDIR_LOCAL/lunaris-mcp"

# Write a manifest with a deliberately WRONG hash (all zeros is not a valid sha256 for this tarball)
cat > "$TMPDIR_LOCAL/manifest.json" <<EOF
{
  "_comment": "test manifest with intentionally wrong hash",
  "$PLAT": "0000000000000000000000000000000000000000000000000000000000000000"
}
EOF

mkdir -p "$TMPDIR_LOCAL/install"

# Invoke postinstall using env-var overrides; production paths are unchanged.
# file:// URL is only valid as a test override via LUNARIS_MCP_TARBALL_URL.
set +e
LUNARIS_MCP_TARBALL_URL="file://$TMPDIR_LOCAL/$PLAT" \
  LUNARIS_MCP_MANIFEST_PATH="$TMPDIR_LOCAL/manifest.json" \
  LUNARIS_MCP_INSTALL_DIR="$TMPDIR_LOCAL/install" \
  node "$PKG_DIR/scripts/postinstall.js"
EXIT_CODE=$?
set -e

if [[ $EXIT_CODE -eq 0 ]]; then
  echo "ERROR: postinstall should have aborted on sha256 mismatch but exited 0" >&2
  exit 1
fi

echo "PASS: postinstall aborted with exit code $EXIT_CODE on sha256 mismatch"
