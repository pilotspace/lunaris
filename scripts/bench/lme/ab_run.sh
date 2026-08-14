#!/usr/bin/env bash
# Measured A/B: graph-off arm, then graph-on arm, over the SAME offsets.
#
# The arms MUST run sequentially and from the same process-per-question
# structure — the control for a graph A/B is the other arm of the SAME run,
# not a number from a previous day. Judge/generation noise is ~+/-5 points
# (proven 2026-07-30: a byte-identical re-run flipped 10 verdicts out of 108),
# so a cross-run comparison cannot resolve anything smaller than that.
#
# Usage:
#   scripts/bench/lme/ab_run.sh --dry-run
#   scripts/bench/lme/ab_run.sh
set -uo pipefail

# shellcheck source=scripts/bench/lme/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

OFFSETS_FILE="${OFFSETS_FILE:-$LME_DIR/questions/offsets125.tsv}"
MOON_PORT="${MOON_PORT:-6399}"
export OFFSETS_FILE MOON_PORT

DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -h|--help) lme_help "${BASH_SOURCE[0]}"; exit 0 ;;
    *) LME_EXIT=2 lme_die "unknown argument '$arg' (see --help)" ;;
  esac
done

# Guard here too: ab_run.sh is an entry point in its own right and must refuse
# a reserved port before it delegates.
lme_guard_port "$MOON_PORT" "MOON_PORT"
lme_guard_url "${MOON_URL:-moon://127.0.0.1:$MOON_PORT}" "MOON_URL"

if [ "$DRY_RUN" = "1" ]; then
  echo "=== A/B dry run — both arms, no question executed ==="
  ARM=graphoff "$LME_DIR/run_lme.sh" --dry-run || exit $?
  echo
  ARM=graphon  "$LME_DIR/run_lme.sh" --dry-run || exit $?
  exit 0
fi

echo "=== measured graphoff start @ $(date +%H:%M:%S) ==="
ARM=graphoff "$LME_DIR/run_lme.sh"
echo "=== measured graphon start @ $(date +%H:%M:%S) ==="
ARM=graphon  "$LME_DIR/run_lme.sh"
echo "=== AB_COMPLETE @ $(date +%H:%M:%S) ==="
