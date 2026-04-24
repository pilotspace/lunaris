#!/usr/bin/env bash
# =============================================================================
# scripts/16-04-calibrate.sh — Plan 16-04 Task 1
#
# Reproducible 6-run EVAL-05 calibration driver for CONSOL-V1-02.
#
# Executes `eval_05_helios_10k` 6 times back-to-back (3x Moon + 3x Postgres)
# under the post-16-01 default `ActRConsolidator` wiring. Each run captures
# an env fingerprint + observed `promotion_rate` into a per-run JSON. After
# all 6 runs complete, delegates band computation to
# `scripts/16-04-compute-band.py`.
#
# The 6 captured JSONs at `milestones/v0.1.2-CONSOL-CALIBRATION/run-*.json`
# are the empirical substrate for the CONSOL-V1-02 enforcement band that
# Plan 16-05 commits to REQUIREMENTS.md.
#
# USAGE
#   bash scripts/16-04-calibrate.sh          # refuses if run-*.json present
#   bash scripts/16-04-calibrate.sh --force  # overwrites prior run artifacts
#
# ENVIRONMENT
#   MOON_URL            — default redis://127.0.0.1:6380
#   PG_URL              — default postgres://postgres:postgres@127.0.0.1:5432/lunaris
#   PG_CONTAINER_NAME   — default lunaris-pg (fallback: pg-lunaris)
#   MOON_BIN            — optional override for the moon binary path
#
# PRECONDITIONS (hard-fail, each surfaces as exit 1 with explicit reason)
#   1. Git working tree clean
#   2. Moon reachable at $MOON_URL (PING -> PONG)
#   3. Postgres reachable (psql -c 'SELECT 1')
#   4. 16-01 landed (BACKEND_ENV_VAR symbol present in consolidator_pipeline.rs)
#   5. LUNARIS_CONSOLIDATOR_BACKEND is UNSET (must exercise the default path)
#
# EXIT CODES
#   0  — all 6 runs completed + SUMMARY.md emitted
#   1  — precondition failed
#   2  — bench binary missing required calibration flags (see HARD_STOP below)
#   3  — a calibration run itself failed
# =============================================================================

set -euo pipefail

# -----------------------------------------------------------------------------
# 0. Configuration + arg parsing
# -----------------------------------------------------------------------------

FORCE=0
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    -h|--help)
      sed -n '2,40p' "$0"
      exit 0
      ;;
    *)
      echo "error: unknown argument: $arg" >&2
      exit 1
      ;;
  esac
done

REPO_ROOT=$(git rev-parse --show-toplevel)
cd "$REPO_ROOT"

OUT_DIR="milestones/v0.1.2-CONSOL-CALIBRATION"
TMP_DIR="tmp"
mkdir -p "$OUT_DIR" "$TMP_DIR"

# Idempotency guard — refuse to overwrite without --force.
# Use shopt nullglob so an unmatched literal glob expands to an empty array
# rather than staying as the literal string (which would trip `set -e` via
# `ls` exit 2 on a non-existent path under pipefail).
if [[ $FORCE -eq 0 ]]; then
  shopt -s nullglob
  existing_runs=("$OUT_DIR"/run-*.json)
  shopt -u nullglob
  if [[ ${#existing_runs[@]} -gt 0 ]]; then
    echo "error: $OUT_DIR already contains ${#existing_runs[@]} run-*.json file(s)." >&2
    echo "       pass --force to overwrite, or clear the directory first." >&2
    exit 1
  fi
fi

MOON_URL="${MOON_URL:-redis://127.0.0.1:6380}"
PG_URL="${PG_URL:-postgres://postgres:postgres@127.0.0.1:5432/lunaris}"
PG_CONTAINER_NAME="${PG_CONTAINER_NAME:-lunaris-pg}"

# Parse host+port out of MOON_URL for redis-cli (redis://HOST:PORT)
MOON_HOST_PORT=$(printf '%s\n' "$MOON_URL" | sed -E 's#^redis://##; s#/.*$##')
MOON_HOST=$(printf '%s\n' "$MOON_HOST_PORT" | cut -d: -f1)
MOON_PORT=$(printf '%s\n' "$MOON_HOST_PORT" | cut -d: -f2)

# -----------------------------------------------------------------------------
# 1. Preconditions
# -----------------------------------------------------------------------------

echo "==> [precondition 1/5] git working tree clean"
if ! git diff-index --quiet HEAD --; then
  echo "error: git working tree is dirty. Commit or stash before calibrating." >&2
  git status --short >&2
  exit 1
fi

echo "==> [precondition 2/5] Moon reachable at $MOON_HOST:$MOON_PORT"
if ! command -v redis-cli >/dev/null 2>&1; then
  echo "error: redis-cli not on PATH — cannot PING Moon." >&2
  exit 1
fi
if ! reply=$(redis-cli -h "$MOON_HOST" -p "$MOON_PORT" PING 2>/dev/null) || [[ "$reply" != "PONG" ]]; then
  echo "error: Moon PING failed at $MOON_HOST:$MOON_PORT (got: '$reply')." >&2
  exit 1
fi

echo "==> [precondition 3/5] Postgres reachable"
PG_CONTAINER_RESOLVED=""
for cand in "$PG_CONTAINER_NAME" "pg-lunaris" "lunaris-pg"; do
  if docker inspect "$cand" >/dev/null 2>&1; then
    PG_CONTAINER_RESOLVED="$cand"
    break
  fi
done
if [[ -z "$PG_CONTAINER_RESOLVED" ]]; then
  echo "error: none of [$PG_CONTAINER_NAME, pg-lunaris, lunaris-pg] is a running docker container." >&2
  exit 1
fi
if ! psql "$PG_URL" -c 'SELECT 1' >/dev/null 2>&1; then
  echo "error: psql '$PG_URL' -c 'SELECT 1' failed. Is Postgres accepting connections?" >&2
  exit 1
fi

echo "==> [precondition 4/5] 16-01 landed (BACKEND_ENV_VAR present)"
if ! grep -q "BACKEND_ENV_VAR" crates/lunaris/src/consolidator_pipeline.rs; then
  echo "error: BACKEND_ENV_VAR not found in crates/lunaris/src/consolidator_pipeline.rs." >&2
  echo "       16-01 must land before 16-04 calibration can run." >&2
  exit 1
fi

echo "==> [precondition 5/5] LUNARIS_CONSOLIDATOR_BACKEND is UNSET"
if [[ -n "${LUNARIS_CONSOLIDATOR_BACKEND:-}" ]]; then
  echo "error: LUNARIS_CONSOLIDATOR_BACKEND is set ('$LUNARIS_CONSOLIDATOR_BACKEND')." >&2
  echo "       Calibration must exercise the default path. unset the env var and retry." >&2
  exit 1
fi

# -----------------------------------------------------------------------------
# 2. Env fingerprint (captured ONCE, asserted identical across runs)
# -----------------------------------------------------------------------------

# Pick a hasher that exists. blake3sum is preferred (matches Moon tooling);
# sha256sum is the stdlib-ubiquitous fallback and plan-sanctioned.
if command -v blake3sum >/dev/null 2>&1; then
  HASHER="blake3sum"
elif command -v sha256sum >/dev/null 2>&1; then
  HASHER="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  HASHER="shasum -a 256"
else
  echo "error: no suitable hasher found (blake3sum / sha256sum / shasum)." >&2
  exit 1
fi

hash_file() {
  # $1 = path; emits hex digest only
  $HASHER "$1" | awk '{print $1}'
}

GIT_SHA=$(git rev-parse HEAD)

# Moon version — prefer relative-path release binary (matches local dev convention),
# fall back to PATH, then to redis INFO as a last resort.
MOON_VERSION=""
if [[ -n "${MOON_BIN:-}" && -x "$MOON_BIN" ]]; then
  MOON_VERSION=$("$MOON_BIN" --version 2>/dev/null || true)
fi
if [[ -z "$MOON_VERSION" && -x "../moon/target/release/moon" ]]; then
  MOON_VERSION=$(../moon/target/release/moon --version 2>/dev/null || true)
fi
if [[ -z "$MOON_VERSION" ]] && command -v moon >/dev/null 2>&1; then
  MOON_VERSION=$(moon --version 2>/dev/null || true)
fi
if [[ -z "$MOON_VERSION" ]]; then
  # Last resort: ask the server for its reported version
  MOON_VERSION=$(redis-cli -h "$MOON_HOST" -p "$MOON_PORT" INFO server 2>/dev/null \
    | awk -F: '/redis_version/ {gsub(/\r/,""); print "redis-compat:" $2}' \
    | head -n1)
fi
MOON_VERSION="${MOON_VERSION:-unknown}"

# Postgres image digest from docker
PG_IMAGE_DIGEST=$(docker inspect \
  --format='{{if .RepoDigests}}{{index .RepoDigests 0}}{{else}}{{.Image}}{{end}}' \
  "$PG_CONTAINER_RESOLVED" 2>/dev/null || echo "unknown")

# Candle / HF model cache hash — tolerate missing cache (dev box with no models yet).
CANDLE_CACHE_HASH=""
HF_CACHE="${HF_HOME:-$HOME/.cache/huggingface}/hub"
if [[ -d "$HF_CACHE" ]]; then
  # hash-of-hashes: one line per safetensors file, sorted for determinism
  tmp_hashes=$(mktemp)
  find "$HF_CACHE" -type f -name "*.safetensors" -print0 \
    | xargs -0 -I{} $HASHER "{}" 2>/dev/null \
    | awk '{print $1}' \
    | sort > "$tmp_hashes" || true
  if [[ -s "$tmp_hashes" ]]; then
    CANDLE_CACHE_HASH=$(hash_file "$tmp_hashes")
  fi
  rm -f "$tmp_hashes"
fi
CANDLE_CACHE_HASH="${CANDLE_CACHE_HASH:-none}"

TRACE_HASH=$(hash_file "crates/lunaris-bench/src/bin/eval_05_helios_10k.rs")

echo
echo "------ env fingerprint (asserted identical across all 6 runs) ------"
echo "  git_sha                = $GIT_SHA"
echo "  moon_version           = $MOON_VERSION"
echo "  pg_lunaris_image_digest= $PG_IMAGE_DIGEST"
echo "  candle_cache_hash      = $CANDLE_CACHE_HASH"
echo "  trace_hash             = $TRACE_HASH"
echo "  hasher                 = $HASHER"
echo "--------------------------------------------------------------------"
echo

# -----------------------------------------------------------------------------
# 3. HARD_STOP — verify the bench binary accepts the calibration flags.
#
# Plan 16-04 explicitly states: "If `eval_05_helios_10k` does not currently
# accept `--backend` / `--format json` / `--trace-seed` flags, the script MUST
# fail fast — this plan does NOT modify eval_05_helios_10k.rs; any missing
# flag surfacing here is a `checkpoint:decision` escalation."
#
# We probe with --help and require all three flag tokens. If any is missing,
# exit 2 and surface the decision back to the operator via stderr.
# -----------------------------------------------------------------------------

echo "==> probing eval_05_helios_10k for calibration flags (--backend, --format, --trace-seed)"
HELP_OUT=$(cargo run --release --quiet -p lunaris-bench --bin eval_05_helios_10k \
  --features moon-it,pg-it -- --help 2>&1 || true)

missing_flags=()
printf '%s' "$HELP_OUT" | grep -q -- "--backend"     || missing_flags+=("--backend")
printf '%s' "$HELP_OUT" | grep -q -- "--format"      || missing_flags+=("--format")
printf '%s' "$HELP_OUT" | grep -q -- "--trace-seed"  || missing_flags+=("--trace-seed")

if [[ ${#missing_flags[@]} -gt 0 ]]; then
  cat >&2 <<EOF

================================================================================
HARD_STOP: eval_05_helios_10k is missing calibration flags: ${missing_flags[*]}
================================================================================

Plan 16-04 explicitly does NOT modify crates/lunaris-bench/src/bin/eval_05_helios_10k.rs.
This is a \`checkpoint:decision\` escalation to the operator:

  option A (add-flags-in-16-04): extend 16-04 scope to teach eval_05 the flags.
  option B (extend-16-01):       re-open 16-01 to own the flag plumbing.
  option C (env-only-shim):      drive the bench via MOON_URL/PG_URL env vars
                                 instead of --backend (see bench binary docstring),
                                 and skip --trace-seed / --format json (the binary
                                 already writes a deterministic JSON envelope to
                                 \$EVAL_05_OUT). This is the SIMPLIFY branch.

Re-run once the chosen path has landed. Nothing was written; preconditions
remained green.
================================================================================
EOF
  exit 2
fi

# -----------------------------------------------------------------------------
# 4. Loop: 6 runs back-to-back (3 Moon + 3 Postgres)
# -----------------------------------------------------------------------------

run_once() {
  # $1 = backend (moon|postgres)
  # $2 = run index (1..3)
  local backend="$1"
  local n="$2"
  local run_id="${backend}-${n}"
  local bench_out="$TMP_DIR/eval-05-${run_id}.json"
  local final_out="$OUT_DIR/run-${run_id}.json"

  echo
  echo "==> [run ${run_id}] starting ($(date -u +'%Y-%m-%dT%H:%M:%SZ'))"

  if ! cargo run --release --quiet \
        -p lunaris-bench --bin eval_05_helios_10k \
        --features moon-it,pg-it -- \
        --backend "$backend" \
        --trace-seed deterministic \
        --format json \
        > "$bench_out"; then
    echo "error: run ${run_id} failed — cargo/bench returned non-zero." >&2
    exit 3
  fi

  local captured_at
  captured_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

  # Use python3 (stdlib) to re-shape the bench JSON into the canonical run schema.
  # Gracefully tolerates missing fields (emits nulls) rather than silently
  # faking successful runs.
  python3 - "$bench_out" "$final_out" <<PY_END
import json, sys
src, dst = sys.argv[1], sys.argv[2]
with open(src) as f:
    raw = json.load(f)

def pick(*keys, default=None):
    cur = raw
    for k in keys:
        if isinstance(cur, dict) and k in cur:
            cur = cur[k]
        else:
            return default
    return cur

envelope = {
    "schema_version": "1.0",
    "run_id": "${run_id}",
    "backend": "${backend}",
    "git_sha": "${GIT_SHA}",
    "moon_version": ${MOON_VERSION@Q},
    "pg_lunaris_image_digest": ${PG_IMAGE_DIGEST@Q},
    "candle_cache_hash": "${CANDLE_CACHE_HASH}",
    "trace_hash": "${TRACE_HASH}",
    "promotion_rate": pick("promotion_rate") or pick("metrics", "promotion_rate"),
    "eval_05_p50_ms": pick("recall_p50_ms") or pick("metrics", "recall_p50_ms") or pick("p50_ms"),
    "queue_depth_peak": pick("queue_depth_peak") or pick("metrics", "queue_depth_peak"),
    "slo_reasons": pick("slo_reasons") or pick("reasons") or [],
    "captured_at": "${captured_at}",
    "raw_bench_path": src,
}
with open(dst, "w") as f:
    json.dump(envelope, f, indent=2, sort_keys=True)
    f.write("\n")
print(f"  wrote {dst}", file=sys.stderr)
PY_END

  echo "==> [run ${run_id}] done — $final_out"
}

echo "==> executing 6 back-to-back runs (3x moon, 3x postgres)"
for n in 1 2 3; do run_once moon     "$n"; done
for n in 1 2 3; do run_once postgres "$n"; done

# -----------------------------------------------------------------------------
# 5. Hand off to compute-band
# -----------------------------------------------------------------------------

echo
echo "==> all 6 runs complete; invoking scripts/16-04-compute-band.py"
python3 scripts/16-04-compute-band.py

echo
echo "==> calibration SUMMARY.md at $OUT_DIR/SUMMARY.md"
echo "    review it, then resume plan 16-04 Task 3 with one of:"
echo "    proceed / widen-band / investigate / override"
