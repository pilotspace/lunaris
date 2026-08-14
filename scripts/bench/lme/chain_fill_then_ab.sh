#!/usr/bin/env bash
# End-to-end chain: extraction-cache fill -> coverage check -> measured A/B.
#
# This is the single command an operator runs for a full N=125 A/B from a
# fresh clone. It is long (see README for wall-clock), so run it detached:
#
#   nohup scripts/bench/lme/chain_fill_then_ab.sh > /dev/null 2>&1 & disown
#
# It also starts the bench Moon watchdog, because on 2026-07-30 the bench Moon
# died silently mid-run and 20 questions burned as SKIPPED before anyone
# looked.
#
# Usage:
#   scripts/bench/lme/chain_fill_then_ab.sh --dry-run
#   scripts/bench/lme/chain_fill_then_ab.sh
set -uo pipefail

# shellcheck source=scripts/bench/lme/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

MOON_PORT="${MOON_PORT:-6399}"
OFFSETS_FILE="${OFFSETS_FILE:-$LME_DIR/questions/offsets125.tsv}"
LOG="${LOG:-$LME_RESULTS_DIR/chain.log}"
SHARDS="${SHARDS:-5}"
export MOON_PORT OFFSETS_FILE SHARDS

DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -h|--help) lme_help "${BASH_SOURCE[0]}"; exit 0 ;;
    *) LME_EXIT=2 lme_die "unknown argument '$arg' (see --help)" ;;
  esac
done

lme_guard_port "$MOON_PORT" "MOON_PORT"
lme_guard_url "${MOON_URL:-moon://127.0.0.1:$MOON_PORT}" "MOON_URL"

TOTAL=$(grep -v '^[[:space:]]*#' "$OFFSETS_FILE" 2>/dev/null | awk 'NF' | wc -l | tr -d ' ')
# Fill coverage floor: the A/B is only worth launching if the cache actually
# covers the question set. 96% of TOTAL, matching the operator's 120/125.
MIN_FILL="${MIN_FILL:-$(( TOTAL * 96 / 100 ))}"

if [ "$DRY_RUN" = "1" ]; then
  echo "=== chain dry run — nothing executed ==="
  echo "  offsets file         : $OFFSETS_FILE ($TOTAL questions)"
  echo "  fill coverage floor  : $MIN_FILL"
  echo "  chain log            : $LOG"
  echo
  "$LME_DIR/fill_cache.sh" --dry-run || exit $?
  echo
  "$LME_DIR/ab_run.sh" --dry-run || exit $?
  exit 0
fi

mkdir -p "$(dirname "$LOG")"
lme_require_api_key

echo "chain: starting bench Moon watchdog @ $(date '+%F %T')" >> "$LOG"
"$LME_DIR/moon_watchdog.sh" >> "$LOG" 2>&1 &
WATCHDOG_PID=$!

echo "chain: fill pass start @ $(date '+%F %T')" >> "$LOG"
"$LME_DIR/fill_cache.sh" >> "$LOG" 2>&1

# Count DONE questions from the per-question logs, NOT from the fill log's
# [OK] lines: a resumed fill `continue`s past already-done questions without
# echoing a FILL line, so an [OK] tally under-counts by exactly the number
# carried over from the previous pass.
ok=$(grep -l '^    EXTRACT_CACHE ' "$LME_RESULTS_DIR"/fill/s*/q*.log 2>/dev/null | wc -l | tr -d ' ')
entries=$(lme_count "$LME_EXTRACT_CACHE_DIR")
echo "chain: fill done OK=$ok cache_entries=$entries @ $(date '+%F %T')" >> "$LOG"

if [ "$ok" -lt "$MIN_FILL" ]; then
  echo "chain: ABORT — fill coverage too low (OK=$ok < $MIN_FILL); not launching A/B" >> "$LOG"
  kill "$WATCHDOG_PID" 2>/dev/null
  exit 1
fi

echo "chain: launching ab_run.sh @ $(date '+%F %T')" >> "$LOG"
"$LME_DIR/ab_run.sh" >> "$LOG" 2>&1
rc=$?
echo "chain: ab_run.sh exited rc=$rc @ $(date '+%F %T')" >> "$LOG"
kill "$WATCHDOG_PID" 2>/dev/null
exit "$rc"
