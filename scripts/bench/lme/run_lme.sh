#!/usr/bin/env bash
# Measured LongMemEval-S runner — ONE ARM, one process per question.
#
# Ported into version control in 0.6.2 from the operator's tmp/validate_ollama
# working copy (2026-07-29). Behaviour is byte-for-byte the same measurement;
# only the personal paths, the key file and the missing port guards changed.
#
# Integrity properties carried over from the 2026-07-28 expert review:
#   H1  a question counts as DONE only if the artifact says PASS|FAIL *and*
#       the log carries a machine-readable LME_VERDICT with a "correct" key.
#       A SKIPPED row or a truncated log never counts (the q102 class).
#   H2  per-question watchdog via alarm(2); a timeout is retried, not scored.
#   H3  three-way tally (correct / wrong / ERR). ERR is NEVER scored wrong.
#   H4  config fingerprint written on first run; a resume against a different
#       config or a different binary aborts instead of mixing runs.
#   H5  LME_VERDICT JSON is the single scoring source of truth.
#
# ONE PROCESS PER QUESTION IS MANDATORY (LUNARIS_EVAL_LME_LIMIT=1).
#   * Correctness: LongMemEval-S requires each question to retrieve from ITS
#     OWN haystack. Moon's StartsWith source filter fails open, so a FLUSHALL
#     between questions is the only sound isolation boundary.
#   * Stability: batching questions into one process leaks Metal buffers and
#     eventually wedges the run.
# Do not "optimise" this into a single long-lived process.
#
# Usage:
#   scripts/bench/lme/run_lme.sh --help
#   scripts/bench/lme/run_lme.sh --dry-run
#   ARM=graphoff scripts/bench/lme/run_lme.sh
#   ARM=graphon  scripts/bench/lme/run_lme.sh      # same offsets, facts-aware
set -uo pipefail

# shellcheck source=scripts/bench/lme/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ARM="${ARM:-graphoff}"                        # graphoff | graphon
OFFSETS_FILE="${OFFSETS_FILE:-$LME_DIR/questions/offsets125.tsv}"
DIR="${DIR:-$LME_RESULTS_DIR/$ARM}"
MAX_ATTEMPTS="${MAX_ATTEMPTS:-3}"
QTIMEOUT="${QTIMEOUT:-1500}"                  # seconds per question attempt
MOON_PORT="${MOON_PORT:-6399}"                # dedicated bench Moon — NEVER 6381
OLLAMA_URL="${OLLAMA_URL:-http://127.0.0.1:11434}"
EMBED_MODEL="${EMBED_MODEL:-granite-embed-r2}"
RERANKER_GGUF="${LUNARIS_RERANKER_GGUF:-$HOME/.lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf}"
GEN_MODEL="${GEN_MODEL:-minimax-m3:cloud}"
JUDGE_MODEL="${JUDGE_MODEL:-minimax-m3:cloud}"
# Optional provider swap (all-or-nothing): CHAT_URL re-points the gen/judge
# Ollama-shaped client (LUNARIS_EVAL_OLLAMA_URL — pick a NON-minimax
# GEN_MODEL/JUDGE_MODEL or the MiniMax native client still wins), and
# EXTRACT_BASE_URL + EXTRACT_MODEL re-point graph-arm extraction at an
# OpenAI-compatible /chat/completions endpoint (e.g. a local Claude-CLI
# bridge). All three land in CFG so the H4 fingerprint + config.env record
# the swap; artifacts from different providers can never silently mix.
CHAT_URL="${CHAT_URL:-}"
EXTRACT_BASE_URL="${EXTRACT_BASE_URL:-}"
EXTRACT_MODEL="${EXTRACT_MODEL:-}"

DRY_RUN="${LME_DRY_RUN:-0}"
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -h|--help)
      lme_help "${BASH_SOURCE[0]}"
      exit 0 ;;
    *) LME_EXIT=2 lme_die "unknown argument '$arg' (see --help)" ;;
  esac
done

# --- GUARDS (run before anything destructive, including in --dry-run) ------
lme_guard_port "$MOON_PORT" "MOON_PORT"
MOON_URL="${MOON_URL:-moon://127.0.0.1:${MOON_PORT}}"
lme_guard_url "$MOON_URL" "MOON_URL"

case "$ARM" in
  graphon)  GRAPH=1 ;;
  graphoff) GRAPH=0 ;;
  *) LME_EXIT=2 lme_die "ARM must be graphon|graphoff, got '$ARM'" ;;
esac

[ -r "$OFFSETS_FILE" ] || LME_EXIT=2 lme_die "OFFSETS_FILE not readable: $OFFSETS_FILE"
OFFSETS=$(grep -v '^[[:space:]]*#' "$OFFSETS_FILE" | awk 'NF {print $1}')
TOTAL=$(echo "$OFFSETS" | wc -w | tr -d ' ')
[ "$TOTAL" -gt 0 ] || LME_EXIT=2 lme_die "OFFSETS_FILE yielded zero offsets: $OFFSETS_FILE"

# The measured config. Every knob is here so the fingerprint (H4) covers it.
declare -a CFG=(
  "MOON_URL=${MOON_URL}"
  # Force the remote-embedder branch: one llama.cpp embedder per process
  # deadlocks on Metal under the one-process-per-question structure.
  "LUNARIS_EMBEDDER_GGUF=/nonexistent-force-remote"
  "LUNARIS_EMBEDDER_OLLAMA_URL=${OLLAMA_URL}"
  "LUNARIS_OLLAMA_MODEL=${EMBED_MODEL}"
  "LUNARIS_RERANKER_GGUF=${RERANKER_GGUF}"
  "LUNARIS_EVAL_CACHE_DIR=${LUNARIS_EVAL_CACHE_DIR}"
  "LUNARIS_EVAL_LME_DATASET=longmemeval_s"
  "LUNARIS_EVAL_LME_JUDGE=1"
  "LUNARIS_EVAL_LME_RERANK=1"
  "LUNARIS_EVAL_LME_GEN_MODEL=${GEN_MODEL}"
  "LUNARIS_EVAL_LME_JUDGE_MODEL=${JUDGE_MODEL}"
  "LUNARIS_EVAL_LME_HYBRID=1"
  "LUNARIS_EVAL_LME_POOL=40"
  "LUNARIS_EVAL_LME_TOPK=60"
  "LUNARIS_EVAL_LME_ADAPTIVE=1"
  "LUNARIS_EVAL_LME_GRAPH=${GRAPH}"
  "LUNARIS_EVAL_LME_LIMIT=1"
  "LUNARIS_EVAL_LME_DEBUG=1"
  "LUNARIS_EMBED_BATCH=8"
  # Content-addressed extraction cache: identical chunks replay extraction
  # from disk instead of re-calling the provider — fills on the first pass,
  # after which the graph arm runs at graph-off speed (20 min -> ~75 s per
  # question). Shared across arms/runs by design: graph-off never extracts,
  # and the cache key embeds the model + prompt template.
  "LUNARIS_EVAL_LME_EXTRACT_CACHE_DIR=${LME_EXTRACT_CACHE_DIR}"
)
# Provider-swap knobs join CFG only when set, so the default MiniMax config
# keeps its historical fingerprint.
[ -n "$CHAT_URL" ] && CFG+=("LUNARIS_EVAL_OLLAMA_URL=${CHAT_URL}")
[ -n "$EXTRACT_BASE_URL" ] && CFG+=("LUNARIS_EVAL_LME_EXTRACT_BASE_URL=${EXTRACT_BASE_URL}")
[ -n "$EXTRACT_MODEL" ] && CFG+=("LUNARIS_EVAL_LME_EXTRACT_MODEL=${EXTRACT_MODEL}")

if [ "$DRY_RUN" = "1" ]; then
  echo "=== LME dry run — no question will be executed ==="
  echo "  arm                  : $ARM (LUNARIS_EVAL_LME_GRAPH=$GRAPH)"
  echo "  offsets file         : $OFFSETS_FILE ($TOTAL questions)"
  echo "  first/last offset    : $(echo "$OFFSETS" | head -1) / $(echo "$OFFSETS" | tail -1)"
  echo "  attempts / timeout   : $MAX_ATTEMPTS x ${QTIMEOUT}s"
  echo "  artifact dir         : $DIR"
  lme_load_api_key
  lme_preflight_report "$MOON_PORT"
  echo "  resolved config:"
  printf '    %s\n' "${CFG[@]}"
  echo "=== dry run OK ==="
  exit 0
fi

lme_require_api_key
[ -x "$LME_EVAL_BIN" ] || LME_EXIT=2 lme_die \
  "eval binary not built: $LME_EVAL_BIN
  cargo build --release -p lunaris-bench --bin lunaris-evals --features embed-remote,llamacpp,metal"
mkdir -p "$DIR" "$LME_EXTRACT_CACHE_DIR"

# H4 — config fingerprint: env config + binary identity + git HEAD.
FP=$( { printf '%s\n' "${CFG[@]}"; \
        shasum -a 256 "$LME_EVAL_BIN" | cut -d' ' -f1; \
        git -C "$REPO_ROOT" rev-parse HEAD; } | shasum -a 256 | cut -d' ' -f1 )
if [ -f "$DIR/config.fp" ]; then
  if [ "$(cat "$DIR/config.fp")" != "$FP" ]; then
    LME_EXIT=3 lme_die \
"$DIR holds artifacts from a DIFFERENT config/binary (fingerprint mismatch).
       Refusing to mix runs. Use a fresh DIR or delete the old artifacts."
  fi
else
  echo "$FP" > "$DIR/config.fp"
  printf '%s\n' "${CFG[@]}" > "$DIR/config.env"   # human-readable copy, no secrets
fi

# H1 — a question is DONE only if: the artifact has status PASS|FAIL (never
# SKIPPED), the log has an LME_VERDICT line carrying a "correct" key (absent
# on judge ERR), and the log records no judge error.
done_clean() { # $1=OUT $2=LOG
  [ -f "$1" ] && [ -f "$2" ] \
    && grep -qE '"status"[[:space:]]*:[[:space:]]*"(PASS|FAIL)"' "$1" 2>/dev/null \
    && grep -E '^LME_VERDICT ' "$2" 2>/dev/null | grep -q '"correct"' \
    && ! grep -q 'judge error' "$2" 2>/dev/null
}

tally() { python3 "$LME_DIR/tally.py" --dir "$DIR" --expected "$TOTAL"; }

n=0
for off in $OFFSETS; do
  n=$((n+1))
  OUT="$DIR/q${off}.json"; LOG="$DIR/q${off}.log"
  if done_clean "$OUT" "$LOG"; then continue; fi
  attempt=0
  while [ "$attempt" -lt "$MAX_ATTEMPTS" ]; do
    attempt=$((attempt+1))
    lme_moon_flush "$MOON_PORT"
    env "${CFG[@]}" \
      RUST_LOG=warn \
      LUNARIS_EVAL_LME_OFFSET="$off" \
      MINIMAX_API_KEY="$MINIMAX_API_KEY" \
      perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$QTIMEOUT" \
      "$LME_EVAL_BIN" longmemeval --output "$OUT" > "$LOG" 2>&1
    rc=$?
    if lme_rc_is_timeout "$rc"; then
      echo "WATCHDOG q$off attempt $attempt: killed after ${QTIMEOUT}s (rc=$rc)" >> "$LOG"
    fi
    if done_clean "$OUT" "$LOG"; then break; fi
    sleep 5
  done
  c=$(grep -oE 'correct=(true|false)' "$LOG" | head -1)
  ev=$(grep -oE 'evidence_recall_hit = (true|false)' "$LOG" | head -1)
  st="ERR"; done_clean "$OUT" "$LOG" && st="OK"
  echo "q$off ($n/$TOTAL) [$st]: ${c:-n/a} | ${ev:-n/a} @ $(date +%H:%M:%S)"
  if [ $((n % 25)) -eq 0 ]; then echo "MILESTONE $n/$TOTAL"; tally; fi
done
echo "RUN_DONE arm=$ARM"
tally
