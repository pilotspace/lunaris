#!/usr/bin/env bash
# Keep the BENCH Moon alive for the duration of a run, and start it if it is
# not up yet.
#
# Why: on 2026-07-30 the bench Moon died silently mid-run and every graph-on
# question failed instantly with "Connection refused" — 20 questions burned as
# SKIPPED before anyone looked. The runner has no restart guard of its own.
#
# SAFETY: the target port goes through lib.sh's reserved-port guard on every
# start, ping and restart. Port 6381 is the live personal memory store and can
# never be managed by this script.
#
# Usage:
#   scripts/bench/lme/moon_watchdog.sh --dry-run
#   scripts/bench/lme/moon_watchdog.sh            # foreground, loops
#   nohup scripts/bench/lme/moon_watchdog.sh >/dev/null 2>&1 & disown
set -uo pipefail

# shellcheck source=scripts/bench/lme/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

MOON_PORT="${MOON_PORT:-6399}"
DATA_DIR="${DATA_DIR:-$LME_RESULTS_DIR/moon$MOON_PORT}"
MOON_LOG="${MOON_LOG:-$LME_RESULTS_DIR/moon$MOON_PORT.log}"
EVENTS="${EVENTS:-$LME_RESULTS_DIR/moon${MOON_PORT}_watchdog.log}"
POLL_SECS="${POLL_SECS:-20}"
# The watchdog exits once this marker appears in the chain log — nothing left
# to protect.
STOP_MARKER_FILE="${STOP_MARKER_FILE:-$LME_RESULTS_DIR/chain.log}"
STOP_MARKER="${STOP_MARKER:-AB_COMPLETE}"

DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -h|--help) lme_help "${BASH_SOURCE[0]}"; exit 0 ;;
    *) LME_EXIT=2 lme_die "unknown argument '$arg' (see --help)" ;;
  esac
done

lme_guard_port "$MOON_PORT" "watchdog MOON_PORT"

if [ "$DRY_RUN" = "1" ]; then
  echo "=== moon watchdog dry run — no Moon started ==="
  echo "  managed port         : $MOON_PORT"
  echo "  data dir             : $DATA_DIR"
  echo "  moon log             : $MOON_LOG"
  echo "  event log            : $EVENTS"
  echo "  poll interval        : ${POLL_SECS}s"
  echo "  stop marker          : '$STOP_MARKER' in $STOP_MARKER_FILE"
  lme_preflight_report "$MOON_PORT"
  echo "=== dry run OK ==="
  exit 0
fi

[ -x "$LME_MOON_BIN" ] || LME_EXIT=2 lme_die "moon binary not found: $LME_MOON_BIN (override LME_MOON_BIN)"
mkdir -p "$LME_RESULTS_DIR"

while true; do
  if grep -q "$STOP_MARKER" "$STOP_MARKER_FILE" 2>/dev/null; then
    echo "watchdog: $STOP_MARKER seen, exiting @ $(date '+%F %T')" >> "$EVENTS"
    exit 0
  fi
  if ! lme_moon_ping "$MOON_PORT"; then
    echo "watchdog: Moon $MOON_PORT DOWN — starting @ $(date '+%F %T')" >> "$EVENTS"
    lme_moon_start "$MOON_PORT" "$DATA_DIR" "$MOON_LOG" >/dev/null
    if lme_moon_ping "$MOON_PORT"; then
      echo "watchdog: Moon $MOON_PORT up @ $(date '+%F %T')" >> "$EVENTS"
    else
      echo "watchdog: Moon $MOON_PORT START FAILED @ $(date '+%F %T')" >> "$EVENTS"
    fi
  fi
  sleep "$POLL_SECS"
done
