#!/usr/bin/env bash
# Restart the job's Moon on an EMPTY data directory, then prove it is empty.
#
# integration.yml runs six `cargo test` steps in sequence. Without this between
# them, a suite can pass on state a previous step wrote — F28 found
# `keyword_bm25` green only because an earlier step had created the `chunks`
# index it searched.
#
# Two things this deliberately does NOT do:
#
#   * It does not FLUSHALL. Measured against Moon 0.8.5: FLUSHALL drops keys
#     and LEAVES FT INDICES STANDING, schema and sticky quantization intact.
#     Index-level leftovers are precisely the F28 defect.
#   * It does not restart without `--dir`. Moon's `--dir` auto-resolves to the
#     platform user-data directory when omitted, so a restart would reopen the
#     same store and reset nothing.
set -euo pipefail

PORT="${MOON_PORT:-6390}"
DIR="${MOON_DATA_DIR:?MOON_DATA_DIR must be set}"
BIN="${MOON_BINARY:?MOON_BINARY must be set}"
LOG="${MOON_LOG:-${RUNNER_TEMP:-/tmp}/moon.log}"

# Kill by PORT, never by binary path: a path-matching pkill also takes down
# any other Moon on the host.
if command -v lsof >/dev/null 2>&1; then
  pids="$(lsof -ti ":$PORT" 2>/dev/null || true)"
elif command -v fuser >/dev/null 2>&1; then
  pids="$(fuser "$PORT/tcp" 2>/dev/null || true)"
else
  pids=""
fi
if [ -n "$pids" ]; then
  # shellcheck disable=SC2086
  kill $pids 2>/dev/null || true
fi
for _ in $(seq 1 20); do
  if ! (echo > "/dev/tcp/localhost/$PORT") >/dev/null 2>&1; then break; fi
  sleep 1
done

rm -rf "$DIR"
mkdir -p "$DIR"
echo "=== moon restart on empty $DIR ===" >> "$LOG"
"$BIN" --port "$PORT" --shards 1 --dir "$DIR" >> "$LOG" 2>&1 &
MOON_PID=$!
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "MOON_PID=$MOON_PID" >> "$GITHUB_ENV"
fi

for i in $(seq 1 30); do
  if (echo > "/dev/tcp/localhost/$PORT") >/dev/null 2>&1; then
    echo "moon reachable after $i attempt(s) (pid $MOON_PID)"
    break
  fi
  if [ "$i" = 30 ]; then
    echo "ERROR: moon not reachable after 60s"; cat "$LOG" || true; exit 1
  fi
  sleep 2
done

# Prove the reset actually reset something. Without this the whole isolation
# claim rests on a setup step, and a setup step is not evidence — the F28
# audit spent two rounds analysing a store it had not created.
if command -v redis-cli >/dev/null 2>&1; then
  size="$(redis-cli -p "$PORT" DBSIZE 2>/dev/null || echo unknown)"
  # Keyed on the SUCCESS marker: every successful FT.INFO reply carries
  # `index_name`. The missing-index error has at least three spellings, and
  # enumerating them is an open set.
  info="$(redis-cli -p "$PORT" FT.INFO chunks 2>&1 || true)"
  if echo "$info" | grep -q "index_name" || [ "$size" != "0" ]; then
    echo "ERROR: Moon on $PORT is not empty after reset (DBSIZE=$size)."
    echo "  FT.INFO chunks -> $(echo "$info" | head -1)"
    echo "  A reset that leaves state behind defeats the isolation this script exists for."
    exit 1
  fi
  echo "reset verified: DBSIZE=0, no chunks index"
else
  echo "WARNING: redis-cli absent — restart done but emptiness NOT verified"
fi
