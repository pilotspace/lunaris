#!/usr/bin/env bash
# scripts/bench-fastembed-vs-baselines.sh — Reproducible A/B bench for the
# Lunaris embedder + reranker hot path across three backends:
#
#     candle   — EmbeddingGemma 300M + bge-reranker-v2-m3 via candle 0.10 (CPU)
#     ollama   — HTTP backend via scripts/ollama-replay-server.py (port 11434)
#     fastembed— ONNX backend via fastembed-rs 5.13.4 + ort 2.0.0-rc.12 (CPU)
#
# Drives the same SQuAD 10k×1k strict-replay corpus through each backend on
# identical hardware and emits a single CSV under
# `tmp/bench-fastembed-vs-baselines-<utc-iso8601>.csv` with one row per
# (backend, metric). Feeds Plan 19-03 Task 2's verdict matrix.
#
# Status (Plan 19-03 Task 1):
#   - Skeleton + --dry-run + --help: SHIPPED
#   - Real-run mode: STUB. Operator hardware + live Moon + SQuAD fixture
#     required (see Hardware prerequisites below). Plan 19-03 Task 2 will
#     fill this in once operator-controlled hardware is scheduled.
#   - --soak <duration>: NOT YET. Plan 20-03 Task 3 extends this script with
#     a continuous-load mode for the 24h v0.2 soak.
#
# Hardware prerequisites (do NOT remove — Plan 19-03 T-19-03-01 mitigation):
#   - Quiet host. Close Slack/Chrome/Docker before running. Plug into wall
#     power on laptops. Same host across all three backends — never compare
#     numbers from different boxes.
#   - >= 16 GB RAM. The candle EmbeddingGemma load alone consumes ~1.2 GB
#     resident; fastembed adds the ORT runtime + ~600 MB ONNX weights.
#   - >= 8 GB free disk under $HOME/.cache for model caches
#     (`~/.cache/lunaris/models/` + `~/.cache/huggingface/`).
#   - Live Moon at moon://127.0.0.1:6380 (per
#     .planning/memory/reference_moon_local_run.md — release build, port 6380).
#   - Live Ollama replay server at http://127.0.0.1:11434 driving the SQuAD
#     fixture (see scripts/ollama-replay-server.py). DO NOT use a production
#     Ollama — replay-server is deterministic; production Ollama is not.
#   - SQuAD 10k×1k strict-replay fixture pre-staged under tmp/squad-replay/
#     by scripts/precompute-embeds.py (one-time setup).
#
# Runtime prerequisites:
#   - bash >= 3.2 (script uses indexed arrays only — no bash-4-isms; macOS
#     stock bash 3.2.57 is sufficient)
#   - Python 3.11+ on PATH (for the Ollama replay server)
#   - /usr/bin/time (BSD on macOS, GNU on Linux — script detects both)
#   - cargo + a built lunaris-bench binary (one per backend; the real-run
#     mode rebuilds with the matching `--features` flag per backend)
#   - shellcheck >= 0.9 (CI gate; run `shellcheck "$0"` before pushing)
#
# Usage:
#   scripts/bench-fastembed-vs-baselines.sh --help
#   scripts/bench-fastembed-vs-baselines.sh --dry-run
#   scripts/bench-fastembed-vs-baselines.sh                # real run — STUB
#
# Exit codes:
#   0 — all three backends PASS (recall@3 + p50/p95/p99 + RSS captured)
#   1 — at least one backend FAIL, or real-run mode invoked (currently STUB),
#       or pre-flight check failed
#   2 — bad arguments / --help mis-use
#
# Refs: .planning/phases/19-fastembed-backend/19-03-PLAN.md (Task 1)
# Refs: .planning/phases/20-fastembed-adoption/20-03-PLAN.md (Task 3 — soak)
# Refs: .planning/memory/reference_lunaris_benchmarks.md (2026-04-23 candle
#       baseline: SQuAD 10k×1k strict-replay p50 86 ms / recall@3 86%)

set -euo pipefail

# ---------------------------------------------------------------------------
# Constants & defaults
# ---------------------------------------------------------------------------
SCRIPT_NAME="$(basename "${BASH_SOURCE[0]}")"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

MOON_URL_DEFAULT="moon://127.0.0.1:6380"
OLLAMA_URL_DEFAULT="http://127.0.0.1:11434"
CORPUS_FIXTURE_DEFAULT="${REPO_ROOT}/tmp/squad-replay"

MOON_URL="${LUNARIS_TEST_MOON_URL:-${MOON_URL_DEFAULT}}"
OLLAMA_URL="${LUNARIS_OLLAMA_URL:-${OLLAMA_URL_DEFAULT}}"
CORPUS_FIXTURE="${LUNARIS_BENCH_CORPUS:-${CORPUS_FIXTURE_DEFAULT}}"

UTC_ISO8601="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_CSV="${REPO_ROOT}/tmp/bench-fastembed-vs-baselines-${UTC_ISO8601}.csv"

BACKENDS=("candle" "ollama" "fastembed")

# ---------------------------------------------------------------------------
# CLI parsing
# ---------------------------------------------------------------------------
DRY_RUN=0
SHOW_HELP=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --help|-h)
      SHOW_HELP=1
      shift
      ;;
    *)
      echo "${SCRIPT_NAME}: unknown argument: $1" >&2
      echo "Run \`${SCRIPT_NAME} --help\` for usage." >&2
      exit 2
      ;;
  esac
done

usage() {
  cat <<EOF
${SCRIPT_NAME} — A/B bench for Lunaris embedder + reranker backends.

USAGE
    ${SCRIPT_NAME} [--dry-run] [--help]

OPTIONS
    --dry-run    Print the planned commands per backend without executing
                 them. Use this to verify the planned matrix before
                 committing operator-hardware time.
    --help, -h   Show this help and exit.

ENVIRONMENT
    LUNARIS_TEST_MOON_URL   Moon endpoint (default: ${MOON_URL_DEFAULT}).
    LUNARIS_OLLAMA_URL      Ollama replay-server endpoint
                            (default: ${OLLAMA_URL_DEFAULT}).
    LUNARIS_BENCH_CORPUS    SQuAD fixture root
                            (default: ${CORPUS_FIXTURE_DEFAULT}).

OUTPUT
    tmp/bench-fastembed-vs-baselines-<utc-iso8601>.csv
    One row per (backend, metric). Columns:
        backend,metric,value,unit,notes

EXIT CODES
    0   All backends PASS.
    1   Backend failure OR real-run mode (currently STUB — see top-of-file).
    2   Bad CLI arguments.

STATUS
    Real-run mode is a STUB pending operator hardware. Plan 19-03 Task 2
    fills this in; until then only --dry-run is functional.
EOF
}

if [[ "${SHOW_HELP}" -eq 1 ]]; then
  usage
  exit 0
fi

# ---------------------------------------------------------------------------
# Pre-flight checks (informational under --dry-run, hard-gate under real run)
# ---------------------------------------------------------------------------
preflight_check() {
  local strict="$1"  # 1 = real run (hard-gate), 0 = dry-run (warn only)
  local missing=0

  echo "=== Pre-flight ==="
  echo "Repo root:       ${REPO_ROOT}"
  echo "Moon URL:        ${MOON_URL}"
  echo "Ollama URL:      ${OLLAMA_URL}"
  echo "Corpus fixture:  ${CORPUS_FIXTURE}"
  echo "Output CSV:      ${OUT_CSV}"
  echo

  # bash >= 3.2 (script only uses indexed arrays; macOS stock bash is fine)
  if [[ "${BASH_VERSINFO[0]:-0}" -lt 3 ]]; then
    echo "MISSING: bash >= 3.2 required (have ${BASH_VERSION})" >&2
    missing=1
  fi

  # /usr/bin/time
  if [[ ! -x /usr/bin/time ]]; then
    echo "MISSING: /usr/bin/time not executable at /usr/bin/time" >&2
    missing=1
  fi

  # cargo
  if ! command -v cargo >/dev/null 2>&1; then
    echo "MISSING: cargo not on PATH" >&2
    missing=1
  fi

  # Python (for replay server)
  if ! command -v python3 >/dev/null 2>&1; then
    echo "MISSING: python3 not on PATH (needed for Ollama replay server)" >&2
    missing=1
  fi

  # Replay server script
  if [[ ! -f "${REPO_ROOT}/scripts/ollama-replay-server.py" ]]; then
    echo "MISSING: scripts/ollama-replay-server.py not found" >&2
    missing=1
  fi

  # Corpus fixture
  if [[ ! -d "${CORPUS_FIXTURE}" ]]; then
    echo "WARN: corpus fixture not present at ${CORPUS_FIXTURE}" >&2
    echo "      Stage it via scripts/precompute-embeds.py before real run." >&2
    [[ "${strict}" -eq 1 ]] && missing=1
  fi

  echo
  if [[ "${missing}" -ne 0 ]]; then
    if [[ "${strict}" -eq 1 ]]; then
      echo "Pre-flight FAILED — fix the items above before re-running." >&2
      return 1
    else
      echo "Pre-flight WARN — some items missing but --dry-run continues."
    fi
  else
    echo "Pre-flight OK."
  fi
  return 0
}

# ---------------------------------------------------------------------------
# Per-backend command planner
#
# Emits the exact shell invocation that the real run would execute for the
# given backend. Output is one line, single-quoted where shell-safe so
# operators can copy/paste the line for ad-hoc reproduction.
# ---------------------------------------------------------------------------
plan_backend_cmd() {
  local backend="$1"
  local features
  local extra_env

  case "${backend}" in
    candle)
      features="candle"
      extra_env="LUNARIS_EMBEDDER_BACKEND=candle LUNARIS_RERANKER_BACKEND=candle"
      ;;
    ollama)
      features="ollama"
      extra_env="LUNARIS_EMBEDDER_BACKEND=ollama LUNARIS_OLLAMA_URL=${OLLAMA_URL}"
      ;;
    fastembed)
      features="fastembed"
      extra_env="LUNARIS_EMBEDDER_BACKEND=fastembed LUNARIS_RERANKER_BACKEND=fastembed"
      ;;
    *)
      echo "plan_backend_cmd: unknown backend: ${backend}" >&2
      return 1
      ;;
  esac

  cat <<EOF
# --- backend: ${backend} ---
# Rebuild lunaris-bench with the matching feature set so we measure the
# real dep graph (no runtime backend-swap — features control compilation).
cargo build -p lunaris-bench --release --no-default-features --features "${features}"

# Run the SQuAD 10k×1k strict-replay corpus and capture per-query latency
# + recall@k against the gold-passage mapping. Resident set size captured
# via /usr/bin/time (-l on BSD/macOS, -v on GNU/Linux).
${extra_env} \\
  LUNARIS_TEST_MOON_URL=${MOON_URL} \\
  LUNARIS_BENCH_CORPUS=${CORPUS_FIXTURE} \\
  /usr/bin/time -l \\
    ./target/release/lunaris-bench \\
      --suite squad-strict-replay \\
      --backend ${backend} \\
      --emit-csv-row \\
      --append-to "${OUT_CSV}"

# Expected CSV rows appended (one per metric):
#   ${backend},recall@1,<value>,ratio,
#   ${backend},recall@3,<value>,ratio,
#   ${backend},recall@10,<value>,ratio,
#   ${backend},p50,<value>,ms,
#   ${backend},p95,<value>,ms,
#   ${backend},p99,<value>,ms,
#   ${backend},wall_time,<value>,s,
#   ${backend},peak_rss,<value>,mb,
EOF
}

# ---------------------------------------------------------------------------
# Dry-run mode — print the plan, do not execute.
# ---------------------------------------------------------------------------
run_dry() {
  echo "=== ${SCRIPT_NAME} — DRY RUN ==="
  echo "Real-run mode is STUB; --dry-run prints the planned matrix."
  echo
  preflight_check 0
  echo
  echo "=== Planned CSV header ==="
  echo "backend,metric,value,unit,notes"
  echo
  echo "=== Planned per-backend commands ==="
  for backend in "${BACKENDS[@]}"; do
    plan_backend_cmd "${backend}"
    echo
  done
  echo "=== Aggregate verdict (would be computed after real run) ==="
  cat <<'EOF'
# Verdict gates (Plan 19-03 must_haves):
#   PASS              if recall@3[fastembed] >= recall@3[ollama] - 0.02
#                     AND p50[fastembed] <= p50[candle]
#   PASS-WITH-CAVEAT  if one gate missed by <= 5% and the other is comfortably ahead
#   FAIL              if both gates missed by > 5%
# Verdict line appended as final CSV row:
#   verdict,recommendation,<PASS|PASS-WITH-CAVEAT|FAIL>,,<prose>
EOF
  echo
  echo "Dry-run complete. No commands executed."
  return 0
}

# ---------------------------------------------------------------------------
# Real-run mode — STUB. Operator hardware required.
# ---------------------------------------------------------------------------
run_real() {
  cat >&2 <<EOF
${SCRIPT_NAME}: real-run mode not implemented.

This bench requires operator-controlled hardware to produce meaningful
numbers (T-19-03-01: noisy-box repudiation risk). Plan 19-03 Task 2 fills
in the real-run path once that hardware is scheduled.

To plan the matrix without executing:
    ${SCRIPT_NAME} --dry-run

For the full prerequisite checklist see the top-of-file comment in
${SCRIPT_NAME}.
EOF
  return 1
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
mkdir -p "${REPO_ROOT}/tmp"

if [[ "${DRY_RUN}" -eq 1 ]]; then
  run_dry
  exit $?
fi

run_real
exit $?
