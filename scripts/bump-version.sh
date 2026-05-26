#!/usr/bin/env bash
# scripts/bump-version.sh — atomic workspace version bump (RELEASE-04)
#
# Usage: scripts/bump-version.sh <semver>
# Example: scripts/bump-version.sh 0.1.1
#
# Updates:
#   1. All Rust crate versions in the workspace via `cargo set-version --workspace`
#      (cargo-edit plugin; run `cargo install cargo-edit` if missing).
#   2. crates/lunaris-py/pyproject.toml [project].version (line-scoped sed).
#   3. crates/lunaris-ts/package.json .version (jq).
#   4. crates/lunaris-mcp-npm/package.json .version (jq).
#
# After editing, asserts all four surfaces match the requested semver.
# Exits non-zero on any parity mismatch.
#
# Version source-of-truth: root Cargo.toml [workspace.package].version.
# Extract: grep -A 20 '\[workspace.package\]' Cargo.toml | grep '^version' | head -1
# Phase 26 Plan 26-01: added crates/lunaris-mcp-npm/package.json surface.
# Phase 26 Plan 26-02 will add crates/lunaris-mcp-py/pyproject.toml surface.

set -euo pipefail

VER="${1:-}"
if [[ -z "$VER" ]]; then
  echo "Usage: $0 <semver>" >&2
  exit 2
fi

# Validate semver shape (major.minor.patch with optional -pre)
if [[ ! "$VER" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.-]+)?$ ]]; then
  echo "ERROR: '$VER' is not a valid semver" >&2
  exit 3
fi

# Prereq tools
if ! command -v jq >/dev/null 2>&1; then
  echo "ERROR: jq not installed" >&2
  exit 127
fi
if ! cargo set-version --help >/dev/null 2>&1; then
  echo "ERROR: cargo set-version not available. Install cargo-edit with: cargo install cargo-edit" >&2
  exit 127
fi

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

echo "-> Bumping Rust workspace to $VER (cargo set-version --workspace $VER)"
cargo set-version --workspace "$VER"

echo "-> Bumping crates/lunaris-py/pyproject.toml [project].version to $VER"
pyproject="crates/lunaris-py/pyproject.toml"
line=$(grep -n '^version = ' "$pyproject" | head -1 | cut -d: -f1)
if [[ -z "$line" ]]; then
  echo "ERROR: could not find version line in $pyproject" >&2
  exit 4
fi
sed -i.bak "${line}s/version = \".*\"/version = \"$VER\"/" "$pyproject"
rm -f "$pyproject.bak"

echo "-> Bumping crates/lunaris-ts/package.json .version to $VER"
pkgjson="crates/lunaris-ts/package.json"
tmp=$(mktemp)
jq --arg v "$VER" '.version = $v' "$pkgjson" > "$tmp"
mv "$tmp" "$pkgjson"

echo "-> Bumping crates/lunaris-mcp-npm/package.json .version to $VER"
mcppkgjson="crates/lunaris-mcp-npm/package.json"
tmp=$(mktemp)
jq --arg v "$VER" '.version = $v' "$mcppkgjson" > "$tmp"
mv "$tmp" "$mcppkgjson"

echo "-> Version parity assertion"
# Rust source-of-truth: root Cargo.toml [workspace.package].version.
rust_ver=$(grep -A 20 '\[workspace.package\]' Cargo.toml | grep '^version' | head -1 | sed 's/version *= *"\(.*\)".*/\1/')
py_ver=$(grep '^version = ' "$pyproject" | head -1 | sed 's/.*"\(.*\)".*/\1/')
ts_ver=$(jq -r '.version' "$pkgjson")
npm_mcp_ver=$(jq -r '.version' "$mcppkgjson")

echo "  Rust (workspace.package): $rust_ver"
echo "  Python (pyproject):       $py_ver"
echo "  TypeScript (package):     $ts_ver"
echo "  npm @lunaris/mcp:         $npm_mcp_ver"

if [[ "$rust_ver" != "$VER" || "$py_ver" != "$VER" || "$ts_ver" != "$VER" || "$npm_mcp_ver" != "$VER" ]]; then
  echo "ERROR: version parity broken" >&2
  exit 5
fi

echo "OK: all four surfaces at $VER"
echo "Next: cargo build --workspace --all-targets --all-features; then follow 13-03-HUMAN-UAT.md"
