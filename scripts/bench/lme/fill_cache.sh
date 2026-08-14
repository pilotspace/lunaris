#!/usr/bin/env bash
# Extraction-cache FILL pass — populate the content-addressed extraction cache
# for the whole question set, in parallel, WITHOUT touching the measured
# config. Run this once before an A/B; afterwards the graph arm runs at
# graph-off speed (~20 min -> ~75 s per question).
#
# Why this is safe to parallelise when the measured run is NOT:
#   * RERANK=0 + JUDGE=0 -> no in-process llama.cpp in these workers at all.
#     The Metal-contention deadlock only bites concurrent in-process
#     llama.cpp; embeddings go to the warm Ollama server (harmless
#     concurrency).
#   * Each shard gets its OWN throwaway Moon (SHARD_PORT_BASE + i, scratch
#     dir) so per-question FLUSHALLs never cross shards. Reserved ports are
#     refused by lib.sh for every shard, not just the primary.
#   * Cache writes are atomic (tmp + rename) — concurrent fillers never tear.
#   * Provider in-flight ceiling = SHARDS x extract concurrency 4; keep
#     SHARDS <= 5 (~20 concurrent) to stay under typical rate limits.
#
# The fill artifacts (q*.json/log) are DISPOSABLE — only the cache matters.
#
# Usage:
#   scripts/bench/lme/fill_cache.sh --dry-run
#   SHARDS=5 scripts/bench/lme/fill_cache.sh
set -uo pipefail

# shellcheck source=scripts/bench/lme/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

SHARDS="${SHARDS:-5}"
OFFSETS_FILE="${OFFSETS_FILE:-$LME_DIR/questions/offsets125.tsv}"
BASE="${BASE:-$LME_RESULTS_DIR/fill}"
QTIMEOUT="${QTIMEOUT:-1800}"
SHARD_PORT_BASE="${SHARD_PORT_BASE:-6410}"
OLLAMA_URL="${OLLAMA_URL:-http://127.0.0.1:11434}"
EMBED_MODEL="${EMBED_MODEL:-granite-embed-r2}"

DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -h|--help) lme_help "${BASH_SOURCE[0]}"; exit 0 ;;
    *) LME_EXIT=2 lme_die "unknown argument '$arg' (see --help)" ;;
  esac
done

# --- GUARDS: every shard port, before any Moon is started ------------------
case "$SHARDS" in ''|*[!0-9]*) LME_EXIT=2 lme_die "SHARDS must be numeric, got '$SHARDS'" ;; esac
[ "$SHARDS" -ge 1 ] || LME_EXIT=2 lme_die "SHARDS must be >= 1"
for s in $(seq 0 $((SHARDS - 1))); do
  lme_guard_port "$((SHARD_PORT_BASE + s))" "fill shard $s"
done

[ -r "$OFFSETS_FILE" ] || LME_EXIT=2 lme_die "OFFSETS_FILE not readable: $OFFSETS_FILE"

if [ "$DRY_RUN" = "1" ]; then
  echo "=== LME fill_cache dry run — no shard will be started ==="
  echo "  offsets file         : $OFFSETS_FILE ($(grep -vc '^[[:space:]]*#' "$OFFSETS_FILE") questions)"
  echo "  shards               : $SHARDS on ports $SHARD_PORT_BASE..$((SHARD_PORT_BASE + SHARDS - 1))"
  echo "  per-question timeout : ${QTIMEOUT}s"
  echo "  scratch dir          : $BASE"
  lme_load_api_key
  lme_preflight_report "$SHARD_PORT_BASE"
  echo "=== dry run OK ==="
  exit 0
fi

lme_require_api_key
[ -x "$LME_EVAL_BIN" ] || LME_EXIT=2 lme_die "eval binary not built: $LME_EVAL_BIN"
[ -x "$LME_MOON_BIN" ] || LME_EXIT=2 lme_die "moon binary not found: $LME_MOON_BIN"
mkdir -p "$BASE" "$LME_EXTRACT_CACHE_DIR"

# Shard the offsets round-robin.
i=0
rm -f "$BASE"/shard*.txt
while read -r off _; do
  case "$off" in ''|\#*) continue ;; esac
  echo "$off" >> "$BASE/shard$((i % SHARDS)).txt"
  i=$((i + 1))
done < "$OFFSETS_FILE"

run_shard() { # $1 = shard index
  local s="$1" port=$((SHARD_PORT_BASE + $1))
  local dir="$BASE/s$s" moondir="$BASE/moon$s"
  mkdir -p "$dir" "$moondir"
  local moon_pid
  moon_pid="$(lme_moon_start "$port" "$moondir" "$moondir/moon.log")"

  while read -r off; do
    local out="$dir/q${off}.json" log="$dir/q${off}.log"
    # Done = the cache-stats line proves ingest (and thus extraction) ran to
    # completion for this question. No judge in fill mode.
    if grep -q '^    EXTRACT_CACHE ' "$log" 2>/dev/null; then continue; fi
    for _attempt in 1 2; do
      lme_moon_flush "$port"
      env \
        MOON_URL="moon://127.0.0.1:${port}" \
        LUNARIS_EMBEDDER_GGUF=/nonexistent-force-remote \
        LUNARIS_EMBEDDER_OLLAMA_URL="$OLLAMA_URL" \
        LUNARIS_OLLAMA_MODEL="$EMBED_MODEL" \
        LUNARIS_EVAL_CACHE_DIR="$LUNARIS_EVAL_CACHE_DIR" \
        LUNARIS_EVAL_LME_DATASET=longmemeval_s \
        LUNARIS_EVAL_LME_JUDGE=0 \
        LUNARIS_EVAL_LME_RERANK=0 \
        LUNARIS_EVAL_LME_HYBRID=1 \
        LUNARIS_EVAL_LME_POOL=40 \
        LUNARIS_EVAL_LME_TOPK=60 \
        LUNARIS_EVAL_LME_GRAPH=1 \
        LUNARIS_EVAL_LME_LIMIT=1 \
        LUNARIS_EVAL_LME_DEBUG=1 \
        LUNARIS_EVAL_LME_EXTRACT_CACHE_DIR="$LME_EXTRACT_CACHE_DIR" \
        LUNARIS_EMBED_BATCH=8 \
        RUST_LOG=warn \
        LUNARIS_EVAL_LME_OFFSET="$off" \
        MINIMAX_API_KEY="$MINIMAX_API_KEY" \
        perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$QTIMEOUT" \
        "$LME_EVAL_BIN" longmemeval --output "$out" > "$log" 2>&1
      grep -q '^    EXTRACT_CACHE ' "$log" 2>/dev/null && break
      sleep 5
    done
    st="ERR"; grep -q '^    EXTRACT_CACHE ' "$log" 2>/dev/null && st="OK"
    echo "FILL s$s q$off [$st] $(grep -m1 '^    EXTRACT_CACHE ' "$log" 2>/dev/null | tr -s ' ') @ $(date +%H:%M:%S)"
  done < "$BASE/shard$s.txt"

  kill "$moon_pid" 2>/dev/null
  echo "FILL_SHARD_DONE s$s"
}

pids=()
for s in $(seq 0 $((SHARDS - 1))); do
  run_shard "$s" &
  pids+=($!)
done
for p in "${pids[@]}"; do wait "$p"; done
echo "FILL_ALL_DONE entries=$(lme_count "$LME_EXTRACT_CACHE_DIR" '*.json')"
