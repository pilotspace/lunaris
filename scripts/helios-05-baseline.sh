#!/usr/bin/env bash
# helios-05-baseline.sh -- Operator-runnable HELIOS-05 Criterion baseline script
#
# Runs the `helios_p50` Criterion bench against live Moon + live Postgres,
# saves `--save-baseline v0.1.2`, copies the baseline to the committed
# milestones directory, extracts p50 from Criterion estimates, writes
# VERDICT.json, and exits 0 (both PASS) or 1 (any FAIL).
#
# Usage:
#   MOON_URL=moon://localhost:6380 PG_URL=postgres://lunaris@localhost/lunaris \
#     bash scripts/helios-05-baseline.sh
#
# Prerequisites:
#   - Live Moon running (e.g., ../moon/target/release/moon on port 6380)
#   - Live Postgres with pg-lunaris extensions
#   - jq installed
#   - Rust toolchain (cargo)

set -euo pipefail

# --------------------------------------------------------------------------
# Exit codes
# --------------------------------------------------------------------------
EXIT_PASS=0
EXIT_FAIL=1
EXIT_PREFLIGHT=2

# --------------------------------------------------------------------------
# 1. Pre-flight checks
# --------------------------------------------------------------------------

# jq is required for JSON parsing
if ! command -v jq &>/dev/null; then
    echo >&2 "FATAL: jq is not installed. Install it (brew install jq / apt install jq) and retry."
    exit $EXIT_PREFLIGHT
fi

# MOON_URL must be set
if [[ -z "${MOON_URL:-}" ]]; then
    echo >&2 "FATAL: MOON_URL env var is not set."
    echo >&2 "  Example: export MOON_URL=moon://localhost:6380"
    exit $EXIT_PREFLIGHT
fi

# PG_URL must be set
if [[ -z "${PG_URL:-}" ]]; then
    echo >&2 "FATAL: PG_URL env var is not set."
    echo >&2 "  Example: export PG_URL=postgres://lunaris@localhost/lunaris"
    exit $EXIT_PREFLIGHT
fi

# Guard against NoopConsolidator — HELIOS-05 evidence requires the real
# ActRConsolidator default (Phase 16). If the operator's environment has
# LUNARIS_CONSOLIDATOR_BACKEND=noop, the bench would run without real
# consolidation, silently invalidating the baseline.
if [[ "${LUNARIS_CONSOLIDATOR_BACKEND:-}" == "noop" ]]; then
    echo >&2 "FATAL: LUNARIS_CONSOLIDATOR_BACKEND=noop detected."
    echo >&2 "  HELIOS-05 evidence requires the real ActRConsolidator backend."
    echo >&2 "  Either unset the variable (default is actr) or set it explicitly:"
    echo >&2 "    export LUNARIS_CONSOLIDATOR_BACKEND=actr"
    exit $EXIT_PREFLIGHT
fi
# Force the default to actr for evidence integrity, even if unset.
export LUNARIS_CONSOLIDATOR_BACKEND=actr

# Redact credentials from URLs for logging (T-17-01-02 mitigation)
redact_url() {
    local url="$1"
    # Strip user:pass@ if present
    echo "$url" | sed -E 's|://[^@]*@|://***@|'
}

MOON_URL_REDACTED=$(redact_url "$MOON_URL")
PG_URL_REDACTED=$(redact_url "$PG_URL")

# Extract host:port from URL for TCP probe
extract_host_port() {
    local url="$1"
    # Handle moon://host:port, postgres://user@host:port/db, etc.
    local without_scheme="${url#*://}"
    # Strip user:pass@ if present
    local without_auth="${without_scheme#*@}"
    # Strip /path
    local host_port="${without_auth%%/*}"
    # Strip ?query
    host_port="${host_port%%\?*}"
    echo "$host_port"
}

tcp_probe() {
    local host_port="$1"
    local host="${host_port%%:*}"
    local port="${host_port##*:}"
    # Default ports if not specified
    if [[ "$host" == "$port" ]]; then
        port=6379  # fallback
    fi
    # 1-second timeout TCP probe
    if command -v nc &>/dev/null; then
        nc -z -w1 "$host" "$port" 2>/dev/null
    elif command -v bash &>/dev/null; then
        timeout 1 bash -c "echo >/dev/tcp/$host/$port" 2>/dev/null
    else
        echo >&2 "WARN: no nc or bash /dev/tcp available; skipping TCP probe"
        return 0
    fi
}

MOON_HP=$(extract_host_port "$MOON_URL")
PG_HP=$(extract_host_port "$PG_URL")

echo >&2 "--- HELIOS-05 Baseline Pre-flight ---"
echo >&2 "  git SHA : $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo >&2 "  date    : $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo >&2 "  uname   : $(uname -a)"
echo >&2 "  MOON_URL: $MOON_URL_REDACTED"
echo >&2 "  PG_URL  : $PG_URL_REDACTED"
echo >&2 ""

if ! tcp_probe "$MOON_HP"; then
    echo >&2 "FATAL: Moon backend unreachable at $MOON_HP (1s TCP probe failed)"
    exit $EXIT_PREFLIGHT
fi
echo >&2 "  Moon TCP probe  : OK ($MOON_HP)"

if ! tcp_probe "$PG_HP"; then
    echo >&2 "FATAL: Postgres backend unreachable at $PG_HP (1s TCP probe failed)"
    exit $EXIT_PREFLIGHT
fi
echo >&2 "  Postgres TCP probe: OK ($PG_HP)"
echo >&2 ""

# --------------------------------------------------------------------------
# 2. Build step (release mode for stable measurements)
# --------------------------------------------------------------------------

echo >&2 "--- Building helios_p50 bench (release) ---"
cargo build -p lunaris-bench --bench helios_p50 --release
echo >&2 "  Build: OK"
echo >&2 ""

# --------------------------------------------------------------------------
# 3. Bench execution
# --------------------------------------------------------------------------

echo >&2 "--- Running Criterion helios_p50 bench (--save-baseline v0.1.2) ---"
echo >&2 "  This may take ~60 seconds per backend..."
echo >&2 ""

MOON_URL="$MOON_URL" PG_URL="$PG_URL" \
    cargo bench -p lunaris-bench --bench helios_p50 -- --save-baseline v0.1.2

echo >&2 ""
echo >&2 "  Bench run: OK"
echo >&2 ""

# --------------------------------------------------------------------------
# 4. Evidence copy
# --------------------------------------------------------------------------

BASELINE_DIR="milestones/v0.1.2-HELIOS-05-BASELINE"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
CRITERION_BASE="$TARGET_DIR/criterion/helios_smoke/helios_p50"

mkdir -p "$BASELINE_DIR/moon"
mkdir -p "$BASELINE_DIR/postgres"

# Copy baseline contents (flatten v0.1.2/ into backend dir)
if [[ -d "$CRITERION_BASE/moon/v0.1.2" ]]; then
    cp -r "$CRITERION_BASE/moon/v0.1.2/." "$BASELINE_DIR/moon/"
    echo >&2 "  Copied Moon baseline to $BASELINE_DIR/moon/"
else
    echo >&2 "WARN: Moon baseline directory not found at $CRITERION_BASE/moon/v0.1.2"
fi

if [[ -d "$CRITERION_BASE/postgres/v0.1.2" ]]; then
    cp -r "$CRITERION_BASE/postgres/v0.1.2/." "$BASELINE_DIR/postgres/"
    echo >&2 "  Copied Postgres baseline to $BASELINE_DIR/postgres/"
else
    echo >&2 "WARN: Postgres baseline directory not found at $CRITERION_BASE/postgres/v0.1.2"
fi

# Copy evidence envelopes (one level up from baseline subdir)
for label in moon postgres; do
    if [[ -f "$CRITERION_BASE/$label/envelope.json" ]]; then
        cp "$CRITERION_BASE/$label/envelope.json" "$BASELINE_DIR/$label/"
        echo >&2 "  Copied $label envelope.json"
    else
        echo >&2 "WARN: $label/envelope.json not found"
    fi
done

echo >&2 ""

# --------------------------------------------------------------------------
# 5. Verdict extraction
# --------------------------------------------------------------------------

MOON_BUDGET_MS=20
PG_BUDGET_MS=25

extract_p50_ms() {
    local estimates_path="$1"
    if [[ ! -f "$estimates_path" ]]; then
        echo "null"
        return 1
    fi
    # median.point_estimate is in nanoseconds; convert to ms
    jq '.median.point_estimate / 1000000' "$estimates_path"
}

MOON_P50_MS=$(extract_p50_ms "$BASELINE_DIR/moon/estimates.json") || true
PG_P50_MS=$(extract_p50_ms "$BASELINE_DIR/postgres/estimates.json") || true

# Determine PASS/FAIL for each backend
moon_pass=false
pg_pass=false

if [[ "$MOON_P50_MS" != "null" ]]; then
    # jq comparison: 1 if p50 <= budget
    moon_pass=$(echo "$MOON_P50_MS $MOON_BUDGET_MS" | awk '{print ($1 <= $2) ? "true" : "false"}')
fi

if [[ "$PG_P50_MS" != "null" ]]; then
    pg_pass=$(echo "$PG_P50_MS $PG_BUDGET_MS" | awk '{print ($1 <= $2) ? "true" : "false"}')
fi

GIT_SHA=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)

# Print verdict table
echo ""
echo "=== HELIOS-05 Verdict ==="
echo ""
printf "%-12s | %-16s | %-10s | %s\n" "Backend" "Measured p50 (ms)" "Budget (ms)" "Result"
printf "%-12s-+-%-16s-+-%-10s-+-%s\n" "------------" "----------------" "----------" "------"
if [[ "$MOON_P50_MS" != "null" ]]; then
    moon_verdict=$( [[ "$moon_pass" == "true" ]] && echo "PASS" || echo "FAIL" )
    printf "%-12s | %16.3f | %10d | %s\n" "moon" "$MOON_P50_MS" "$MOON_BUDGET_MS" "$moon_verdict"
else
    printf "%-12s | %16s | %10d | %s\n" "moon" "N/A" "$MOON_BUDGET_MS" "MISSING"
fi
if [[ "$PG_P50_MS" != "null" ]]; then
    pg_verdict=$( [[ "$pg_pass" == "true" ]] && echo "PASS" || echo "FAIL" )
    printf "%-12s | %16.3f | %10d | %s\n" "postgres" "$PG_P50_MS" "$PG_BUDGET_MS" "$pg_verdict"
else
    printf "%-12s | %16s | %10d | %s\n" "postgres" "N/A" "$PG_BUDGET_MS" "MISSING"
fi
echo ""

# Write VERDICT.json
jq -n \
    --argjson moon_p50 "${MOON_P50_MS:-null}" \
    --argjson pg_p50 "${PG_P50_MS:-null}" \
    --argjson moon_pass "$moon_pass" \
    --argjson pg_pass "$pg_pass" \
    --arg git_sha "$GIT_SHA" \
    --arg timestamp "$TIMESTAMP" \
    '{
        moon_p50_ms: $moon_p50,
        postgres_p50_ms: $pg_p50,
        moon_pass: $moon_pass,
        postgres_pass: $pg_pass,
        git_sha: $git_sha,
        timestamp: $timestamp
    }' > "$BASELINE_DIR/VERDICT.json"

echo >&2 "  VERDICT.json written to $BASELINE_DIR/VERDICT.json"
echo >&2 ""

# --------------------------------------------------------------------------
# 6. Exit code
# --------------------------------------------------------------------------

if [[ "$moon_pass" == "true" && "$pg_pass" == "true" ]]; then
    echo "HELIOS-05 BASELINE: PASS (both backends within budget)"
    exit $EXIT_PASS
else
    echo >&2 "HELIOS-05 BASELINE: FAIL (one or more backends exceeded budget)"
    exit $EXIT_FAIL
fi
