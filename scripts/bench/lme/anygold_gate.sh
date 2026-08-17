#!/usr/bin/env bash
# anygold_gate.sh — judge-free retrieval-quality ratchet (GA-2a, the gate
# behind .github/workflows/recall-ratchet.yml).
#
# Measures LongMemEval-S evidence-recall **any-gold** (a gold-evidence
# session present in the capped reader context) over a small fixed offsets
# manifest, one process per question, against a scratch Moon THIS SCRIPT
# STARTS AND OWNS. No LLM judge, no extraction provider, no API key:
# graph-OFF ingest+recall needs only the embedder + reranker GGUFs and the
# dataset cache. That is what makes this the one recall metric a hosted CI
# runner can actually produce.
#
# The ratchet: the run must be FINAL (full coverage, zero ERR) and within
# `tolerance_questions` of the checked-in baseline
# (scripts/bench/lme/baselines/ci-anygold.json). The baseline records the
# retrieval config signature it was measured under; a signature mismatch
# refuses to gate instead of comparing apples to oranges.
#
# Moon ownership: this script REFUSES a port that already answers PING — it
# only ever flushes a Moon it launched itself (and it launches on the bench
# default 6399; 6381 is hard-refused by lib.sh before anything else runs).
#
# Usage:
#   scripts/bench/lme/anygold_gate.sh --dry-run
#   scripts/bench/lme/anygold_gate.sh                                # measure only
#   scripts/bench/lme/anygold_gate.sh --baseline <file>              # CI gate
#   scripts/bench/lme/anygold_gate.sh --write-baseline <file>        # bless
#
# Environment (all optional): OFFSETS_FILE (default questions/offsets16.tsv),
# MOON_PORT (default 6399), DIR, MAX_ATTEMPTS, QTIMEOUT, RERANK (default 1),
# LUNARIS_EMBEDDER_GGUF, LUNARIS_RERANKER_GGUF, LME_EVAL_BIN, LME_MOON_BIN,
# LUNARIS_EVAL_CACHE_DIR.
#
# Sharding (CI wall-clock): SHARD_INDEX=<i> SHARD_COUNT=<n> runs only every
# n-th offset starting at row i (round-robin, so the manifest's category
# stratification spreads across shards). A sharded invocation measures ONLY
# — the ratchet comparison happens after the shards' artifact dirs are
# merged (tally.py --anygold --baseline over the union). The config
# signature is computed from the FULL manifest either way, so shard
# artifacts merge cleanly and match an unsharded baseline.
#
# Exit codes: 0 ok / within tolerance; 1 any-gold regression beyond
# tolerance; 2 bad config; 3 artifact dir belongs to a different
# config/binary; 4 reserved-port refusal; 5 run not final; 6 baseline
# config-signature mismatch; 7 infrastructure failure (dataset download,
# Moon, eval binary SKIP).
set -uo pipefail

# shellcheck source=scripts/bench/lme/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

OFFSETS_FILE="${OFFSETS_FILE:-$LME_DIR/questions/offsets16.tsv}"
DIR="${DIR:-$LME_RESULTS_DIR/anygold}"
MAX_ATTEMPTS="${MAX_ATTEMPTS:-2}"
QTIMEOUT="${QTIMEOUT:-1200}"                 # seconds per question attempt
MOON_PORT="${MOON_PORT:-6399}"               # dedicated bench Moon — NEVER 6381
RERANK="${RERANK:-1}"
SHARD_INDEX="${SHARD_INDEX:-0}"
SHARD_COUNT="${SHARD_COUNT:-1}"
EMBEDDER_GGUF="${LUNARIS_EMBEDDER_GGUF:-$HOME/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf}"
RERANKER_GGUF="${LUNARIS_RERANKER_GGUF:-$HOME/.lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf}"

DRY_RUN="${LME_DRY_RUN:-0}"
BASELINE=""
WRITE_BASELINE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --baseline) BASELINE="${2:?--baseline needs a file}"; shift 2 ;;
    --write-baseline) WRITE_BASELINE="${2:?--write-baseline needs a file}"; shift 2 ;;
    -h|--help)
      lme_help "${BASH_SOURCE[0]}"
      exit 0 ;;
    *) LME_EXIT=2 lme_die "unknown argument '$1' (see --help)" ;;
  esac
done
[ -n "$BASELINE" ] && [ -n "$WRITE_BASELINE" ] \
  && LME_EXIT=2 lme_die "--baseline and --write-baseline are mutually exclusive"

# --- GUARDS (before anything destructive, including in --dry-run) ----------
lme_guard_port "$MOON_PORT" "MOON_PORT"
MOON_URL="${MOON_URL:-moon://127.0.0.1:${MOON_PORT}}"
lme_guard_url "$MOON_URL" "MOON_URL"

[ -r "$OFFSETS_FILE" ] || LME_EXIT=2 lme_die "OFFSETS_FILE not readable: $OFFSETS_FILE"
OFFSETS=$(grep -v '^[[:space:]]*#' "$OFFSETS_FILE" | awk 'NF {print $1}')
TOTAL=$(echo "$OFFSETS" | wc -w | tr -d ' ')
[ "$TOTAL" -gt 0 ] || LME_EXIT=2 lme_die "OFFSETS_FILE yielded zero offsets: $OFFSETS_FILE"

# Shard selection (round-robin over manifest rows). TOTAL/SIG stay computed
# from the FULL manifest so shard artifacts merge into one baseline-
# comparable set.
case "$SHARD_INDEX$SHARD_COUNT" in
  *[!0-9]*) LME_EXIT=2 lme_die "SHARD_INDEX/SHARD_COUNT must be numeric" ;;
esac
[ "$SHARD_COUNT" -ge 1 ] || LME_EXIT=2 lme_die "SHARD_COUNT must be >= 1"
[ "$SHARD_INDEX" -lt "$SHARD_COUNT" ] || LME_EXIT=2 lme_die \
  "SHARD_INDEX ($SHARD_INDEX) must be < SHARD_COUNT ($SHARD_COUNT)"
RUN_OFFSETS=$(echo "$OFFSETS" | awk -v i="$SHARD_INDEX" -v n="$SHARD_COUNT" \
  '((NR-1) % n) == i')
RUN_TOTAL=$(echo "$RUN_OFFSETS" | wc -w | tr -d ' ')
if [ "$SHARD_COUNT" -gt 1 ]; then
  [ -z "$BASELINE" ] && [ -z "$WRITE_BASELINE" ] || LME_EXIT=2 lme_die \
"a sharded invocation measures only — merge the shard artifact dirs and run
  tally.py --anygold --baseline over the union (see recall-ratchet.yml)."
  [ "$RUN_TOTAL" -gt 0 ] || LME_EXIT=2 lme_die \
    "shard $SHARD_INDEX/$SHARD_COUNT selected zero offsets from $OFFSETS_FILE"
fi

# The measured config. Judge-free on purpose: LUNARIS_EVAL_LME_JUDGE stays
# unset, so lunaris-evals runs the cheap evidence-recall pass and needs no
# provider key. Everything else mirrors run_lme.sh's production retrieval
# shape (hybrid vector+BM25 fusion, pool 40 / top-k 60, intent-adaptive
# session capping, rerank), except the embedder: CI has no warm Ollama, so
# the gate uses the IN-PROCESS llama.cpp embedder. One process per question
# keeps that safe — the Metal-contention deadlock lane doesn't exist on CPU
# CI runners, and each process exits before state can accumulate.
declare -a CFG=(
  "MOON_URL=${MOON_URL}"
  "LUNARIS_EMBEDDER_GGUF=${EMBEDDER_GGUF}"
  "LUNARIS_RERANKER_GGUF=${RERANKER_GGUF}"
  "LUNARIS_EVAL_CACHE_DIR=${LUNARIS_EVAL_CACHE_DIR}"
  "LUNARIS_EVAL_LME_DATASET=longmemeval_s"
  "LUNARIS_EVAL_LME_RERANK=${RERANK}"
  "LUNARIS_EVAL_LME_HYBRID=1"
  "LUNARIS_EVAL_LME_POOL=40"
  "LUNARIS_EVAL_LME_TOPK=60"
  "LUNARIS_EVAL_LME_ADAPTIVE=1"
  "LUNARIS_EVAL_LME_GRAPH=0"
  "LUNARIS_EVAL_LME_LIMIT=1"
  "LUNARIS_EVAL_LME_DEBUG=1"
  "LUNARIS_EMBED_BATCH=8"
)

# Machine-independent retrieval-config signature. The baseline stores it;
# tally.py refuses (exit 6) to ratchet across a mismatch. Model files are
# identified by basename — the same GGUF lives at different absolute paths
# on the baseline box and the CI runner.
SIG="anygold-v1|dataset=longmemeval_s|hybrid=1|pool=40|topk=60|adaptive=1"
SIG="$SIG|rerank=${RERANK}|graph=0|embedder=$(basename "$EMBEDDER_GGUF")"
SIG="$SIG|reranker=$(basename "$RERANKER_GGUF")|offsets=$(basename "$OFFSETS_FILE"):${TOTAL}"

if [ "$DRY_RUN" = "1" ]; then
  echo "=== anygold gate dry run — no question will be executed ==="
  echo "  offsets file         : $OFFSETS_FILE ($TOTAL questions)"
  echo "  shard                : $SHARD_INDEX/$SHARD_COUNT ($RUN_TOTAL questions this run)"
  echo "  attempts / timeout   : $MAX_ATTEMPTS x ${QTIMEOUT}s"
  echo "  artifact dir         : $DIR"
  echo "  baseline             : ${BASELINE:-'(none — measure only)'}"
  echo "  write-baseline       : ${WRITE_BASELINE:-'(none)'}"
  echo "  config signature     : $SIG"
  lme_preflight_report "$MOON_PORT"
  echo "  (judge-free: MINIMAX_API_KEY is NOT required for this gate)"
  echo "=== dry run OK ==="
  exit 0
fi

[ -x "$LME_EVAL_BIN" ] || LME_EXIT=2 lme_die \
  "eval binary not built: $LME_EVAL_BIN
  cargo build --release -p lunaris-bench --bin lunaris-evals --features llamacpp"
[ -x "$LME_MOON_BIN" ] || LME_EXIT=2 lme_die "moon binary not found: $LME_MOON_BIN (set LME_MOON_BIN)"
[ -s "$EMBEDDER_GGUF" ] || LME_EXIT=2 lme_die "embedder GGUF missing: $EMBEDDER_GGUF"
if [ "$RERANK" = "1" ]; then
  [ -s "$RERANKER_GGUF" ] || LME_EXIT=2 lme_die "reranker GGUF missing: $RERANKER_GGUF (or set RERANK=0)"
fi
[ -n "$BASELINE" ] && { [ -r "$BASELINE" ] || LME_EXIT=2 lme_die "baseline not readable: $BASELINE"; }

mkdir -p "$DIR"

# Config fingerprint (run_lme.sh H4, gate flavor): signature + binary hash.
# A resume into artifacts from a different config or binary aborts instead
# of silently mixing measurements. macOS ships shasum (perl), most Linux CI
# images ship sha256sum (coreutils) — take whichever exists.
sha256_of() {
  if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$@"; else sha256sum "$@"; fi
}
FP=$( { echo "$SIG"; sha256_of "$LME_EVAL_BIN" | cut -d' ' -f1; } | sha256_of | cut -d' ' -f1 )
if [ -f "$DIR/config.fp" ]; then
  [ "$(cat "$DIR/config.fp")" = "$FP" ] || LME_EXIT=3 lme_die \
"$DIR holds artifacts from a DIFFERENT config/binary (fingerprint mismatch).
  Refusing to mix runs. Delete $DIR (it is throwaway) or use a fresh DIR."
else
  echo "$FP" > "$DIR/config.fp"
  { echo "SIG=$SIG"; printf '%s\n' "${CFG[@]}"; } > "$DIR/config.env"
fi

# --- Moon lifecycle: start, own, and tear down a scratch Moon ---------------
# Refuse a port that already answers: this gate FLUSHALLs between questions
# and must never do that to a store somebody else started.
if lme_moon_ping "$MOON_PORT"; then
  LME_EXIT=2 lme_die \
"something already answers PING on port $MOON_PORT.
  This gate only runs against a scratch Moon it started itself (it flushes
  between questions). Stop the other process or pick another MOON_PORT."
fi
MOON_PID="$(lme_moon_start "$MOON_PORT" "$DIR/moon-data" "$DIR/moon.log")"
cleanup() { kill "$MOON_PID" 2>/dev/null || true; }
trap cleanup EXIT
trap 'exit 130' INT TERM   # EXIT trap then tears the Moon down
lme_moon_ping "$MOON_PORT" || {
  tail -20 "$DIR/moon.log" >&2 || true
  LME_EXIT=7 lme_die "scratch Moon failed to come up on port $MOON_PORT (log above)"
}
lme_log "scratch Moon started on port $MOON_PORT (pid $MOON_PID)"

# A question is DONE when its artifact says PASS|FAIL (never SKIPPED) and its
# log carries a scoreable any-gold trace (evidence_recall_hit debug line or
# LME_VERDICT payload).
done_anygold() { # $1=OUT $2=LOG
  [ -f "$1" ] && [ -f "$2" ] \
    && grep -qE '"status"[[:space:]]*:[[:space:]]*"(PASS|FAIL)"' "$1" 2>/dev/null \
    && grep -qE 'evidence_recall_hit["[:space:]]*[:=][[:space:]]*(true|false)' "$2" 2>/dev/null
}

# An infra SKIP (dataset download failure, unreachable Moon, parse error) is
# loud and terminal: retries won't produce a number and burning the rest of
# the manifest against 20-minute timeouts helps nobody. The reason line is
# printed by EvalRow::skipped on stderr (the per-question log); the JSON row
# carries only status.
skip_reason() { # $1=LOG
  { [ -f "$1" ] && grep -m1 '^SKIPPED ' "$1" 2>/dev/null; } || echo "(no SKIPPED line — see the q*.log)"
}

n=0
fail_streak=0
for off in $RUN_OFFSETS; do
  n=$((n+1))
  OUT="$DIR/q${off}.json"; LOG="$DIR/q${off}.log"
  if done_anygold "$OUT" "$LOG"; then continue; fi
  q_start=$(date +%s)
  attempt=0
  while [ "$attempt" -lt "$MAX_ATTEMPTS" ]; do
    attempt=$((attempt+1))
    lme_moon_flush "$MOON_PORT"
    env "${CFG[@]}" \
      RUST_LOG=warn \
      LUNARIS_EVAL_LME_OFFSET="$off" \
      perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$QTIMEOUT" \
      "$LME_EVAL_BIN" longmemeval --output "$OUT" > "$LOG" 2>&1
    rc=$?
    if lme_rc_is_timeout "$rc"; then
      echo "WATCHDOG q$off attempt $attempt: killed after ${QTIMEOUT}s (rc=$rc)" >> "$LOG"
    fi
    if done_anygold "$OUT" "$LOG"; then break; fi
    if grep -q '"status"[[:space:]]*:[[:space:]]*"SKIPPED"' "$OUT" 2>/dev/null; then
      LME_EXIT=7 lme_die \
"q$off: lunaris-evals SKIPPED instead of scoring — infrastructure failure.
  Reason: $(skip_reason "$LOG")
  Most common causes: LongMemEval-S dataset download failed (network/HF
  outage — the dataset is public, no credentials needed), or the scratch
  Moon died (see $DIR/moon.log). The gate refuses to report a number it
  did not measure."
    fi
    sleep 3
  done
  ev=$(grep -oE 'evidence_recall_hit = (true|false)' "$LOG" | tail -1)
  st="ERR"
  if done_anygold "$OUT" "$LOG"; then st="OK"; fail_streak=0; else fail_streak=$((fail_streak+1)); fi
  echo "q$off ($n/$RUN_TOTAL) [$st]: ${ev:-n/a} | $(( $(date +%s) - q_start ))s @ $(date +%H:%M:%S)"
  if [ "$fail_streak" -ge 2 ]; then
    LME_EXIT=7 lme_die \
"two consecutive questions produced no verdict — infrastructure problem, not
  a recall measurement. See $DIR/q*.log and $DIR/moon.log."
  fi
done
echo "GATE_RUN_DONE offsets=$(basename "$OFFSETS_FILE") shard=$SHARD_INDEX/$SHARD_COUNT total=$RUN_TOTAL"

# --- score + ratchet --------------------------------------------------------
# A sharded run tallies its own slice (RUN_TOTAL) for visibility; the merged
# baseline comparison against the full manifest happens post-merge.
declare -a TALLY_ARGS=(--dir "$DIR" --expected "$RUN_TOTAL" --anygold)
[ -n "$BASELINE" ] && TALLY_ARGS+=(--baseline "$BASELINE" --config-signature "$SIG")
[ -n "$WRITE_BASELINE" ] && TALLY_ARGS+=(--write-baseline "$WRITE_BASELINE" --config-signature "$SIG")

python3 "$LME_DIR/tally.py" "${TALLY_ARGS[@]}" --json | tee "$DIR/anygold.json"
tally_rc=${PIPESTATUS[0]}
python3 "$LME_DIR/tally.py" --dir "$DIR" --expected "$RUN_TOTAL" --anygold
exit "$tally_rc"
