#!/usr/bin/env bash
# 0001-chunks-add-source-tag.sh — PERF-MOON-01
#
# Migrates the Moon `chunks` FT index to include `source` as a TAG field.
# Safe: FT.DROPINDEX (no DD flag) preserves all hash keys.
# Lunaris's ensure_indexes (on next connect) recreates with the new schema.
#
# Usage:
#   ./scripts/moon-migrations/0001-chunks-add-source-tag.sh [--rollback] [--backfill] [--dry-run]
#
# Options:
#   --rollback   Drop the current index so ensure_indexes recreates with
#                whatever schema is compiled into the current Lunaris binary.
#                Use this to revert if the new schema causes issues.
#   --backfill   After migration, SCAN all chunks:* keys and HSET the
#                `source` field from the `meta` JSON blob. Required for
#                pre-migration data to be filterable by @source:{...}.
#   --dry-run    Print what would happen without executing.
#
# Idempotent: checks FT.INFO chunks for the presence of `source` TAG field.
#             If already present, exits with success (no-op).
#
# Prerequisites:
#   - redis-cli on PATH
#   - Moon running at MOON_HOST:MOON_PORT (default 127.0.0.1:6380)

set -euo pipefail

MOON_HOST="${MOON_HOST:-127.0.0.1}"
MOON_PORT="${MOON_PORT:-6380}"
CLI="redis-cli -h $MOON_HOST -p $MOON_PORT"

ROLLBACK=false
BACKFILL=false
DRY_RUN=false

for arg in "$@"; do
  case "$arg" in
    --rollback) ROLLBACK=true ;;
    --backfill) BACKFILL=true ;;
    --dry-run)  DRY_RUN=true ;;
    *) echo "Unknown option: $arg"; exit 1 ;;
  esac
done

log() { echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] $*"; }

# ---- Connectivity check ----
if ! $CLI PING 2>/dev/null | grep -q PONG; then
  echo "ERROR: Cannot reach Moon at $MOON_HOST:$MOON_PORT"
  exit 1
fi

# ---- Rollback mode ----
if $ROLLBACK; then
  log "ROLLBACK: Dropping chunks FT index (hash keys preserved)"
  if $DRY_RUN; then
    log "DRY-RUN: Would execute FT.DROPINDEX chunks"
  else
    $CLI FT.DROPINDEX chunks 2>/dev/null || true
    log "ROLLBACK complete. Next Lunaris connect will recreate with compiled schema."
  fi
  exit 0
fi

# ---- Idempotent check ----
# Parse FT.INFO chunks output for the source TAG field.
# If source TAG is already present, this migration is a no-op.
FT_INFO=$($CLI FT.INFO chunks 2>/dev/null || echo "NO_INDEX")

if echo "$FT_INFO" | grep -qi "source.*TAG\|TAG.*source"; then
  log "IDEMPOTENT: chunks index already has source TAG field. No-op."
  exit 0
fi

# ---- Migration ----
log "Migrating chunks FT index to add source TAG field..."
log "Step 1: FT.DROPINDEX chunks (preserves hash keys, drops index only)"

if $DRY_RUN; then
  log "DRY-RUN: Would execute FT.DROPINDEX chunks"
  log "DRY-RUN: Next Lunaris connect recreates index with source TAG"
  if $BACKFILL; then
    log "DRY-RUN: Would SCAN chunks:* and HSET source from meta JSON"
  fi
  exit 0
fi

# Drop the index (NOT the data -- no DD flag)
$CLI FT.DROPINDEX chunks 2>/dev/null || {
  # If index doesn't exist, that's fine -- ensure_indexes will create it fresh
  log "Note: chunks index did not exist (fresh deployment). ensure_indexes will create."
}

log "Step 2: Index dropped. Next Lunaris MoonStorage::connect() will recreate"
log "        with SchemaField::Tag(\"source\") via ensure_indexes."
log ""
log "IMPORTANT: Start or restart Lunaris to trigger ensure_indexes."
log "           e.g., cargo run -- --moon-url moon://127.0.0.1:6380"

# ---- Optional backfill ----
if $BACKFILL; then
  log "Step 3: Backfilling source field on existing chunks:* keys..."
  CURSOR=0
  BACKFILLED=0
  SKIPPED=0
  while true; do
    # SCAN for chunks:* keys
    RESULT=$($CLI SCAN "$CURSOR" MATCH "chunks:*" COUNT 100)
    CURSOR=$(echo "$RESULT" | head -1)
    KEYS=$(echo "$RESULT" | tail -n +2)

    for KEY in $KEYS; do
      [ -z "$KEY" ] && continue
      # Read meta JSON, extract source
      META=$($CLI HGET "$KEY" meta 2>/dev/null || echo "")
      if [ -z "$META" ]; then
        SKIPPED=$((SKIPPED + 1))
        continue
      fi
      # Extract source from JSON (simple grep -- assumes flat JSON with "source":"value")
      SOURCE=$(echo "$META" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('source',''))" 2>/dev/null || echo "")
      if [ -n "$SOURCE" ]; then
        $CLI HSET "$KEY" source "$SOURCE" >/dev/null
        BACKFILLED=$((BACKFILLED + 1))
      else
        SKIPPED=$((SKIPPED + 1))
      fi
    done

    [ "$CURSOR" = "0" ] && break
  done
  log "Backfill complete: $BACKFILLED keys updated, $SKIPPED keys skipped (no source in meta)"
fi

log "Migration 0001-chunks-add-source-tag complete."
log "Verify: redis-cli -p $MOON_PORT FT.INFO chunks | grep -i source"
