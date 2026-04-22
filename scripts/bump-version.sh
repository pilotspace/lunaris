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
#
# After editing, asserts all three surfaces match the requested semver.
# Exits non-zero on any parity mismatch.
#
# Parity read-back: the Rust source-of-truth is crates/lunaris/Cargo.toml
# (the umbrella crate wrapped by both the Python and TypeScript host crates).
# This repo does NOT carry a [workspace.package].version field — per-crate
# [package].version is the canonical shape. Plan 13-03 deviation Rule 1.

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

echo "-> Version parity assertion"
# Rust source-of-truth: umbrella crate (the one py+ts wrap).
rust_ver=$(grep '^version' crates/lunaris/Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
py_ver=$(grep '^version = ' "$pyproject" | head -1 | sed 's/.*"\(.*\)".*/\1/')
ts_ver=$(jq -r '.version' "$pkgjson")

echo "  Rust (crates/lunaris): $rust_ver"
echo "  Python (pyproject):    $py_ver"
echo "  TypeScript (package):  $ts_ver"

if [[ "$rust_ver" != "$VER" || "$py_ver" != "$VER" || "$ts_ver" != "$VER" ]]; then
  echo "ERROR: version parity broken" >&2
  exit 5
fi

echo "OK: all three surfaces at $VER"
echo "Next: cargo build --workspace --all-targets --all-features; then follow 13-03-HUMAN-UAT.md"
