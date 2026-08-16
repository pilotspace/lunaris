#!/usr/bin/env bash
# Measured PersonaMem runner — ONE ARM, one process per SHARED CONTEXT.
#
# PersonaMem's unit of work is a shared context, not a question: the ~6-20
# questions of one context share ONE incremental, append-only ingest of that
# persona's interaction history. Splitting them across processes would re-ingest
# the whole prefix per question for no isolation benefit — the isolation
# boundary that matters is BETWEEN contexts (one persona's memories must never
# answer another persona's question), and that is a FLUSHALL, done here before
# every context and again inside the harness.
#
# Integrity properties, carried over from the LongMemEval expert review:
#   H1  a context counts as DONE only if the artifact says PASS|FAIL *and* the
#       log carries PM_RUN_DONE with "errors":0. A SKIPPED row, a truncated log
#       or a context with an unresolved chat error never counts.
#   H2  per-context watchdog via alarm(2); a timeout is retried, not scored.
#   H3  three-way tally (correct / wrong / ERR). ERR is NEVER scored wrong.
#   H4  config fingerprint written on first run; a resume against a different
#       config or a different binary aborts instead of mixing runs.
#   H5  PM_VERDICT JSON is the single scoring source of truth.
#
# TEMPORAL HONESTY is enforced in the harness, not here: each question sees
# exactly messages[..end_index_in_shared_context] because the store is only
# ever appended to, and a hit from beyond that prefix marks the question ERR.
#
# Usage:
#   scripts/bench/pm/run_pm.sh --help
#   scripts/bench/pm/run_pm.sh --dry-run
#   scripts/bench/pm/run_pm.sh                       # memory arm, 32k split
#   ARM=nomem scripts/bench/pm/run_pm.sh             # no-memory floor arm
#   SPLIT=128k OFFSETS_FILE=... scripts/bench/pm/run_pm.sh
#   OFFSETS_FILE=scripts/bench/pm/contexts/offsets_smoke.tsv QLIMIT=2 \
#     scripts/bench/pm/run_pm.sh                     # 2-question smoke
set -uo pipefail

# shellcheck source=scripts/bench/pm/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ARM="${ARM:-memory}"                          # memory | nomem
SPLIT="${SPLIT:-32k}"                         # 32k | 128k | 1M
OFFSETS_FILE="${OFFSETS_FILE:-$PM_DIR/contexts/offsets_32k.tsv}"
DIR="${DIR:-$PM_RESULTS_DIR/$SPLIT-$ARM}"
MAX_ATTEMPTS="${MAX_ATTEMPTS:-3}"
QTIMEOUT="${QTIMEOUT:-3600}"                  # seconds per context attempt
MOON_PORT="${MOON_PORT:-6399}"                # dedicated bench Moon — NEVER 6379/6380/6381
OLLAMA_URL="${OLLAMA_URL:-http://127.0.0.1:11434}"     # embedder (granite-embed-r2)
CHAT_URL="${CHAT_URL:-http://127.0.0.1:11435}"         # reader bridge (Ollama-shaped)
EMBED_MODEL="${EMBED_MODEL:-granite-embed-r2}"
RERANKER_GGUF="${LUNARIS_RERANKER_GGUF:-$HOME/.lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf}"
READER_MODEL="${READER_MODEL:-claude-sonnet-5}"
TOPK="${TOPK:-10}"
POOL="${POOL:-30}"
QOFFSET="${QOFFSET:-0}"                       # question slice within a context
QLIMIT="${QLIMIT:-}"                          # empty = every question

DRY_RUN="${PM_DRY_RUN:-0}"
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -h|--help)
      pm_help "${BASH_SOURCE[0]}"
      exit 0 ;;
    *) PM_EXIT=2 pm_die "unknown argument '$arg' (see --help)" ;;
  esac
done

# --- GUARDS (run before anything destructive, including in --dry-run) ------
pm_guard_port "$MOON_PORT" "MOON_PORT"
MOON_URL="${MOON_URL:-moon://127.0.0.1:${MOON_PORT}}"
pm_guard_url "$MOON_URL" "MOON_URL"

case "$ARM" in
  memory) NOMEM=0 ;;
  nomem)  NOMEM=1 ;;
  *) PM_EXIT=2 pm_die "ARM must be memory|nomem, got '$ARM'" ;;
esac
case "$SPLIT" in
  32k|128k|1M) ;;
  *) PM_EXIT=2 pm_die "SPLIT must be 32k|128k|1M, got '$SPLIT'" ;;
esac

[ -r "$OFFSETS_FILE" ] || PM_EXIT=2 pm_die "OFFSETS_FILE not readable: $OFFSETS_FILE"
OFFSETS=$(grep -v '^[[:space:]]*#' "$OFFSETS_FILE" | awk 'NF {print $1}')
TOTAL=$(echo "$OFFSETS" | wc -w | tr -d ' ')
[ "$TOTAL" -gt 0 ] || PM_EXIT=2 pm_die "OFFSETS_FILE yielded zero offsets: $OFFSETS_FILE"

# The measured config. Every knob is here so the fingerprint (H4) covers it.
declare -a CFG=(
  "MOON_URL=${MOON_URL}"
  # Force the remote-embedder branch: one in-process llama.cpp embedder per
  # process deadlocks on Metal under GPU contention.
  "LUNARIS_EMBEDDER_GGUF=/nonexistent-force-remote"
  "LUNARIS_EMBEDDER_OLLAMA_URL=${OLLAMA_URL}"
  "LUNARIS_OLLAMA_MODEL=${EMBED_MODEL}"
  "LUNARIS_RERANKER_GGUF=${RERANKER_GGUF}"
  "LUNARIS_EVAL_CACHE_DIR=${LUNARIS_EVAL_CACHE_DIR}"
  "LUNARIS_EVAL_OLLAMA_URL=${CHAT_URL}"
  "LUNARIS_EVAL_PM_SPLIT=${SPLIT}"
  "LUNARIS_EVAL_PM_MODEL=${READER_MODEL}"
  "LUNARIS_EVAL_PM_LIMIT=1"
  "LUNARIS_EVAL_PM_QOFFSET=${QOFFSET}"
  "LUNARIS_EVAL_PM_TOPK=${TOPK}"
  "LUNARIS_EVAL_PM_POOL=${POOL}"
  "LUNARIS_EVAL_PM_HYBRID=1"
  "LUNARIS_EVAL_PM_RERANK=1"
  "LUNARIS_EVAL_PM_NOMEM=${NOMEM}"
  "LUNARIS_EVAL_PM_DEBUG=1"
  "LUNARIS_EMBED_BATCH=8"
)
[ -n "$QLIMIT" ] && CFG+=("LUNARIS_EVAL_PM_QLIMIT=${QLIMIT}")

if [ "$DRY_RUN" = "1" ]; then
  echo "=== PersonaMem dry run — no context will be executed ==="
  echo "  arm                  : $ARM (LUNARIS_EVAL_PM_NOMEM=$NOMEM)"
  echo "  split                : $SPLIT"
  echo "  reader model         : $READER_MODEL via $CHAT_URL"
  echo "  offsets file         : $OFFSETS_FILE ($TOTAL contexts)"
  echo "  first/last offset    : $(echo "$OFFSETS" | head -1) / $(echo "$OFFSETS" | tail -1)"
  echo "  attempts / timeout   : $MAX_ATTEMPTS x ${QTIMEOUT}s"
  echo "  artifact dir         : $DIR"
  pm_load_api_key
  pm_preflight_report "$MOON_PORT"
  echo "  resolved config:"
  printf '    %s\n' "${CFG[@]}"
  echo "=== dry run OK ==="
  exit 0
fi

pm_require_reader_credentials "$READER_MODEL"
[ -x "$PM_EVAL_BIN" ] || PM_EXIT=2 pm_die \
  "eval binary not built: $PM_EVAL_BIN
  cargo build --release -p lunaris-bench --bin lunaris-evals --features embed-remote,llamacpp,metal"
mkdir -p "$DIR"

# H4 — config fingerprint: env config + binary identity + git HEAD.
FP=$( { printf '%s\n' "${CFG[@]}"; \
        shasum -a 256 "$PM_EVAL_BIN" | cut -d' ' -f1; \
        git -C "$REPO_ROOT" rev-parse HEAD; } | shasum -a 256 | cut -d' ' -f1 )
if [ -f "$DIR/config.fp" ]; then
  if [ "$(cat "$DIR/config.fp")" != "$FP" ]; then
    PM_EXIT=3 pm_die \
"$DIR holds artifacts from a DIFFERENT config/binary (fingerprint mismatch).
       Refusing to mix runs. Use a fresh DIR or delete the old artifacts."
  fi
else
  echo "$FP" > "$DIR/config.fp"
  printf '%s\n' "${CFG[@]}" > "$DIR/config.env"   # human-readable copy, no secrets
fi

# H1 — a context is DONE only if: the eval artifact has status PASS|FAIL (never
# SKIPPED) and the log carries the harness's completion sentinel with zero
# unresolved chat/recall errors.
done_clean() { # $1=OUT $2=LOG
  [ -f "$1" ] && [ -f "$2" ] \
    && grep -qE '"status"[[:space:]]*:[[:space:]]*"(PASS|FAIL)"' "$1" 2>/dev/null \
    && grep -E '^PM_RUN_DONE ' "$2" 2>/dev/null | tail -1 | grep -q '"errors":0'
}

tally() { python3 "$PM_DIR/tally.py" --dir "$DIR" --expected "$TOTAL"; }

n=0
for off in $OFFSETS; do
  n=$((n+1))
  OUT="$DIR/c${off}.json"; LOG="$DIR/c${off}.log"
  if done_clean "$OUT" "$LOG"; then continue; fi
  attempt=0
  while [ "$attempt" -lt "$MAX_ATTEMPTS" ]; do
    attempt=$((attempt+1))
    pm_moon_flush "$MOON_PORT"
    env "${CFG[@]}" \
      RUST_LOG=warn \
      LUNARIS_EVAL_PM_OFFSET="$off" \
      LUNARIS_EVAL_PM_ARTIFACT_DIR="$DIR/questions/c${off}" \
      MINIMAX_API_KEY="${MINIMAX_API_KEY:-}" \
      perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$QTIMEOUT" \
      "$PM_EVAL_BIN" personamem --output "$OUT" > "$LOG" 2>&1
    rc=$?
    if pm_rc_is_timeout "$rc"; then
      echo "WATCHDOG c$off attempt $attempt: killed after ${QTIMEOUT}s (rc=$rc)" >> "$LOG"
    fi
    if done_clean "$OUT" "$LOG"; then break; fi
    sleep 5
  done
  summary=$(grep -E '^PM_RUN_DONE ' "$LOG" 2>/dev/null | tail -1)
  st="ERR"; done_clean "$OUT" "$LOG" && st="OK"
  echo "c$off ($n/$TOTAL) [$st]: ${summary:-no PM_RUN_DONE} @ $(date +%H:%M:%S)"
  if [ $((n % 10)) -eq 0 ]; then echo "MILESTONE $n/$TOTAL"; tally; fi
done
echo "RUN_DONE arm=$ARM split=$SPLIT"
tally
