#!/usr/bin/env bash
# Deploy the long-lived Lunaris daemons to ~/.lunaris/bin.
#
# WHY THIS EXISTS (2026-07-19): daemons launched from target/release/ get
# their mapped text pages invalidated by the next `cargo build --release` —
# macOS then either SIGKILLs them (codesigning) or leaves a thread
# fault-looping at 100% CPU on an unreadable page (the lunaris-mcp zombie,
# 54 CPU-minutes of system time). Long-lived processes must run from a
# stable path that rebuilds never touch.
#
# The copy is staged + `mv`-ed (never `cp` over the destination): rewriting
# a signed Mach-O in place invalidates its code-signature cache and the
# next launch dies with OS_REASON_CODESIGNING (seen on the Moon 6381
# restart, launchd runs 1-3).
#
# Build is Metal-enabled: without `lunaris/metal` the embedder loads with
# n_gpu_layers=0 and every embedding burns CPU threads while the GPU idles
# (`offloaded 0/23 layers` in the boot log). Opt out for a CPU-only box
# with LUNARIS_DEPLOY_NO_METAL=1.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${LUNARIS_BIN_DIR:-$HOME/.lunaris/bin}"
FEATURES=(--features lunaris/metal)
if [[ "${LUNARIS_DEPLOY_NO_METAL:-0}" == "1" ]]; then
  FEATURES=()
fi

echo "==> building lunaris-hook (lunaris-hook + lunaris-contextd) and lunaris-mcp ${FEATURES[*]:-<no metal>}"
cargo build --release --manifest-path "$ROOT/Cargo.toml" \
  -p lunaris-hook -p lunaris-mcp "${FEATURES[@]}"

mkdir -p "$DEST"
for bin in lunaris-hook lunaris-contextd lunaris-mcp; do
  src="$ROOT/target/release/$bin"
  [[ -x "$src" ]] || { echo "!! missing build artifact: $src" >&2; exit 1; }
  staged="$DEST/.$bin.staged.$$"
  cp "$src" "$staged"
  mv -f "$staged" "$DEST/$bin"   # atomic swap; never overwrites in place
  echo "==> deployed $DEST/$bin"
done

# Cycle the running contextd so the next hook event spawns the deployed
# build. The adapter (scripts/lunaris-codex-hook-adapter.py) resolves
# ~/.lunaris/bin first, so the respawn picks up this deploy.
if pkill -f 'lunaris-contextd --socket' 2>/dev/null; then
  rm -f "$HOME/.lunaris/codex-contextd.sock"
  echo "==> cycled lunaris-contextd (respawns on next hook event from $DEST)"
else
  echo "==> no running lunaris-contextd to cycle"
fi

echo "==> done. Reconnect MCP clients (e.g. /mcp in Claude Code) to pick up $DEST/lunaris-mcp"
