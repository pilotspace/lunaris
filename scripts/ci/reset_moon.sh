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
# `lsof`/`fuser` are not guaranteed on a runner, and an empty `$pids` used to
# skip the kill silently. The launch step exports MOON_PID; use it as the
# fallback rather than proceeding with nothing to kill.
if [ -z "$pids" ] && [ -n "${MOON_PID:-}" ]; then
  pids="$MOON_PID"
fi

if [ -n "$pids" ]; then
  # shellcheck disable=SC2086
  kill $pids 2>/dev/null || true
fi

# Wait on PROCESS LIVENESS, not on a port probe.
#
# The probe this replaces polled `/dev/tcp/localhost/$PORT` and broke the
# moment one connect attempt failed. A connect can fail transiently while the
# server is still very much alive (a momentarily full accept backlog is
# enough), and when it did, the script carried on: `rm -rf "$DIR"` ran under a
# live Moon, the replacement could not bind the port, and the verification
# below was answered by the OLD server.
#
# That is not hypothetical. In the failing run every healthy reset logged
# "moon reachable after 2 attempt(s)"; the broken one logged **1** — answered
# instantly, because nothing had gone away — and then `DBSIZE ':8'`.
#
# `kill -0` asks the kernel whether the process exists. There is no transient
# false negative.
alive_pids() {
  local p out=""
  for p in $pids; do
    if kill -0 "$p" 2>/dev/null; then out="$out $p"; fi
  done
  printf '%s' "$out"
}

if [ -n "$pids" ]; then
  for _ in $(seq 1 20); do
    [ -z "$(alive_pids)" ] && break
    sleep 1
  done
  if [ -n "$(alive_pids)" ]; then
    stubborn="$(alive_pids)"
    echo "moon pid(s)$stubborn ignored SIGTERM after 20s; escalating to SIGKILL"
    # shellcheck disable=SC2086
    kill -9 $stubborn 2>/dev/null || true
    for _ in $(seq 1 10); do
      [ -z "$(alive_pids)" ] && break
      sleep 1
    done
  fi
  if [ -n "$(alive_pids)" ]; then
    echo "ERROR: moon pid(s)$(alive_pids) survived SIGTERM and SIGKILL."
    exit 1
  fi
fi

# Belt and braces: even with every known pid reaped, refuse to delete the data
# directory while anything still holds the port. Deleting it under a live
# server is what turns this reset into a no-op that still reports success.
if (echo > "/dev/tcp/localhost/$PORT") >/dev/null 2>&1; then
  echo "ERROR: port $PORT is still accepting connections after the old Moon was reaped."
  echo "  Something else is listening. Refusing to reset around it, because every"
  echo "  check below would then be answered by a store this script did not create."
  exit 1
fi

rm -rf "$DIR"
mkdir -p "$DIR"
echo "=== moon restart on empty $DIR ===" >> "$LOG"
# MOON_MAX_UNFLUSHED_SEGMENTS — opt-in, and deliberately NOT the default.
#
# Moon stalls every foreground write with `MOONERR busy: compaction backlog`
# once `--max-unflushed-immutable-segments` (default 20) immutable vector
# segments pile up. On Moon 0.8.5 that backlog can never drain under sustained
# ingest: the background merge's recall verifier returns exactly 0.0000, aborts,
# and backs off 60s -> 120s -> 240s forever. The store then has no in-band
# recovery — `FT.CONFIG SET <idx> MERGE_RECALL_TOLERANCE 0`, the remedy Moon's
# own log recommends, is itself a foreground write and is stalled too, and
# `FT.COMPACT` returns the backlog error despite `shard/segment_stall.rs`
# documenting it as exempt. Measured on a clean Linux runner and twice on macOS.
#
# Setting this to 0 disables ONLY the write-stall backpressure. It does not
# touch the recall verifier, so no merge is waved through and no index is
# silently degraded — segments simply accumulate. That is acceptable for a
# throwaway benchmark store and is NOT acceptable for the integration suites,
# which is why this is opt-in per workflow rather than a change to the default.
EXTRA_ARGS=()
if [ -n "${MOON_MAX_UNFLUSHED_SEGMENTS:-}" ]; then
  EXTRA_ARGS+=(--max-unflushed-immutable-segments "$MOON_MAX_UNFLUSHED_SEGMENTS")
  echo "reset_moon: --max-unflushed-immutable-segments $MOON_MAX_UNFLUSHED_SEGMENTS"
fi

"$BIN" --port "$PORT" --shards 1 --dir "$DIR" "${EXTRA_ARGS[@]}" >> "$LOG" 2>&1 &
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
# claim rests on a setup step, and a setup step is not evidence — the F28 audit
# spent two rounds analysing a store it had not created.
#
# Spoken over a raw socket, NOT via redis-cli. The first version of this script
# guarded the check with `command -v redis-cli` and warned when absent; GitHub
# runners have no redis-cli, so all six invocations skipped verification and the
# job went green anyway. A check with a soft-fail escape hatch is the exact
# defect this script exists to prevent, so there is no escape hatch now: if the
# store cannot be proven empty, the step FAILS.
probe() {
  exec 3<>"/dev/tcp/localhost/$PORT" || return 1
  printf 'DBSIZE\r\nFT.INFO chunks\r\n' >&3
  local line out=""
  while IFS= read -r -t 3 line <&3; do
    out+="$line"$'\n'
  done
  exec 3<&- 2>/dev/null || true
  exec 3>&- 2>/dev/null || true
  printf '%s' "$out"
}

reply="$(probe)" || {
  echo "ERROR: could not open a socket to Moon on $PORT to verify the reset"
  exit 1
}

# DBSIZE answers with a RESP integer, `:0`.
size_line="$(printf '%s' "$reply" | head -1 | tr -d '\r')"
if [ "$size_line" != ":0" ]; then
  echo "ERROR: Moon on $PORT is not empty after reset — DBSIZE replied '$size_line', wanted ':0'."
  echo "  A reset that leaves state behind defeats the isolation this script exists for."
  exit 1
fi

# FT.INFO on a missing index answers with a RESP error. Keyed on the SUCCESS
# marker `index_name`, which every successful reply carries: Moon spells the
# missing-index error at least three ways ("no such index", "Unknown index",
# "Unknown Index name") and enumerating them is an open set.
#
# Matched with a bash `case`, not `grep`. `if ... | grep -q ...` is FAIL-OPEN:
# when grep is missing the pipeline exits non-zero, the condition is false, and
# the script falls straight through to "reset verified" having checked nothing.
# That is the same shape as the `command -v redis-cli` escape hatch this
# script's header describes, so it does not get to survive next to that note.
# A `case` needs no external command and cannot fail this way.
case "$reply" in
  *index_name*)
    echo "ERROR: the \`chunks\` FT index survived the reset on $PORT."
    echo "  FLUSHALL leaves indices standing; only an empty --dir removes them."
    exit 1
    ;;
esac

echo "reset verified: DBSIZE=0, no chunks index"
