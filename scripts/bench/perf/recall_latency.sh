#!/usr/bin/env bash
# GA-2b — recall-latency envelope runner.
#
# Stands up its OWN scratch Moon, ingests the deterministic 100k-doc corpus
# through the real public ingest paths, and runs the timed recall passes for
# the three production configs on the GA-1 unified production root:
#
#   baseline  graph OFF, rerank OFF        (the shipped default)
#   rerank    LUNARIS_RECALL_RERANK stage  (real bge GGUF in the loop)
#   graph     graph pipeline ON            (chunks ∧ facts legs)
#
# Usage:
#   scripts/bench/perf/recall_latency.sh all            # the one-command gate
#   scripts/bench/perf/recall_latency.sh up             # start scratch Moon
#   scripts/bench/perf/recall_latency.sh ingest [S E]   # docs [S, E), resumable
#   scripts/bench/perf/recall_latency.sh measure <cfg>  # baseline|rerank|graph
#   scripts/bench/perf/recall_latency.sh down           # stop scratch Moon
#
# Env knobs:
#   GA2B_PORT      scratch Moon port      (default 6399; 6379/6380/6381 REFUSED)
#   GA2B_DOCS      corpus size            (default 100000)
#   GA2B_QUERIES   timed queries          (default 500)
#   GA2B_WARMUP    warmup queries         (default 50)
#   GA2B_MOON_BIN  moon binary            (default ../moon/target/release/moon)
#   GA2B_OUT_DIR   results dir            (default target/ga2b)
#
# PORT SAFETY (mirrors scripts/bench/lme/lib.sh GUARD 1): 6381 is the
# operator's live personal memory store; 6379/6380 are system/dev stores.
# This runner refuses them, only ever FLUSHes nothing (fresh --dir every
# `up`), and only kills the Moon IT started (pidfile-scoped).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(git -C "$here" rev-parse --show-toplevel 2>/dev/null || (cd "$here/../../.." && pwd))}"

PORT="${GA2B_PORT:-6399}"
DOCS="${GA2B_DOCS:-100000}"
QUERIES="${GA2B_QUERIES:-500}"
WARMUP="${GA2B_WARMUP:-50}"
MOON_BIN="${GA2B_MOON_BIN:-$REPO_ROOT/../moon/target/release/moon}"
OUT_DIR="${GA2B_OUT_DIR:-$REPO_ROOT/target/ga2b}"
RUN_DIR="$OUT_DIR/moon-$PORT"
PIDFILE="$RUN_DIR/moon.pid"
URL="moon://127.0.0.1:$PORT"
BIN="$REPO_ROOT/target/release/recall-latency"

die() { echo "FATAL: $*" >&2; exit 2; }
log() { echo "[ga2b] $*" >&2; }

guard_port() {
  case "$PORT" in
    ''|*[!0-9]*) die "GA2B_PORT must be numeric, got '$PORT'" ;;
  esac
  for reserved in 6379 6380 6381; do
    [ "$PORT" = "$reserved" ] && die "port $PORT is RESERVED (live dev/personal store) — use 6399+"
  done
  return 0
}

# Version-agnostic liveness probe (same shape as lme_moon_ping): a plain
# RESP PING via redis-cli, bounded at 3 s.
moon_ping() { redis-cli -p "$PORT" -t 3 ping >/dev/null 2>&1; }

moon_version() {
  redis-cli -p "$PORT" -t 3 info server 2>/dev/null | tr -d '\r' \
    | awk -F: '/^(moon_version|redis_version)/ {print $1"="$2}' | paste -sd' ' -
}

ensure_bin() {
  [ -x "$BIN" ] || die "missing $BIN — build first:
  cargo build --release -p lunaris-bench --features llamacpp,metal --bin recall-latency"
}

cmd_up() {
  guard_port
  [ -x "$MOON_BIN" ] || die "moon binary not found/executable: $MOON_BIN (set GA2B_MOON_BIN)"
  if moon_ping; then die "something already answers PING on $PORT — refusing to reuse a store this runner did not start"; fi
  rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR/data"
  # --max-unflushed-immutable-segments 4096 mirrors the production launchd
  # units: the default cap trips "busy: compaction backlog" under the
  # corpus-build write rate (~1.3k docs/s) and aborts ingest.
  nohup "$MOON_BIN" --bind 127.0.0.1 --port "$PORT" --dir "$RUN_DIR/data" --shards 1 \
    --protected-mode no --disk-free-min-pct 1 --max-unflushed-immutable-segments 4096 \
    >"$RUN_DIR/moon.log" 2>&1 &
  echo $! > "$PIDFILE"
  for _ in $(seq 1 30); do moon_ping && break; sleep 1; done
  moon_ping || { cat "$RUN_DIR/moon.log" >&2; die "Moon on $PORT did not answer PING within 30s"; }
  log "Moon up on $PORT (pid $(cat "$PIDFILE"), $(moon_version))"
}

cmd_ingest() {
  guard_port; ensure_bin
  moon_ping || die "no Moon answering on $PORT — run 'up' first"
  local start="${1:-0}" end="${2:-$DOCS}"
  log "ingest docs [$start, $end) of $DOCS"
  "$BIN" --moon-url "$URL" ingest --start "$start" --end "$end"
}

cmd_measure() {
  guard_port; ensure_bin
  moon_ping || die "no Moon answering on $PORT — run 'up' first"
  local cfg="${1:?usage: measure baseline|rerank|graph [tag]}"
  local tag="${2:+-$2}"
  mkdir -p "$OUT_DIR"
  log "measure config=$cfg queries=$QUERIES warmup=$WARMUP offset=${GA2B_QUERY_OFFSET:-0}"
  "$BIN" --moon-url "$URL" measure --config "$cfg" \
    --queries "$QUERIES" --warmup "$WARMUP" --docs-hint "$DOCS" \
    --query-offset "${GA2B_QUERY_OFFSET:-0}" \
    --out "$OUT_DIR/recall-latency-$cfg$tag.json" \
    --dump-samples "$OUT_DIR/recall-latency-$cfg$tag.samples"
}

cmd_down() {
  guard_port
  if [ -f "$PIDFILE" ]; then
    local pid; pid="$(cat "$PIDFILE")"
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      log "stopped Moon pid $pid (port $PORT)"
    fi
    rm -f "$PIDFILE"
  else
    log "no pidfile at $PIDFILE — nothing this runner started is running"
  fi
}

cmd_all() {
  cmd_up
  trap cmd_down EXIT
  # Chunked so progress is visible; the corpus is deterministic in the doc
  # index, so chunking does not change the result.
  local chunk=20000 s=0
  while [ "$s" -lt "$DOCS" ]; do
    local e=$((s + chunk)); [ "$e" -gt "$DOCS" ] && e="$DOCS"
    cmd_ingest "$s" "$e"
    s="$e"
  done
  for cfg in baseline rerank graph; do
    cmd_measure "$cfg"
  done
  log "results in $OUT_DIR/recall-latency-{baseline,rerank,graph}.json"
}

case "${1:-}" in
  up)      cmd_up ;;
  ingest)  shift; cmd_ingest "$@" ;;
  measure) shift; cmd_measure "$@" ;;
  down)    cmd_down ;;
  all)     cmd_all ;;
  *)       awk 'NR==1 {next} /^#/ {sub(/^# ?/, ""); print; next} {exit}' "$0"; exit 2 ;;
esac
