#!/usr/bin/env bash
# scripts/sdk-real-evidence.sh — exercise lunaris-py + lunaris-ts against a
# LIVE Moon (and optionally Postgres) backend, emitting a per-scenario JSON
# evidence envelope under `milestones/v0.1.1-sdk-real/`.
#
# Usage:
#   LUNARIS_MOON_URL=moon://host:6380 \
#   [LUNARIS_TEST_POSTGRES_URL=postgres://user:pw@host/db] \
#   scripts/sdk-real-evidence.sh
#
# Exit codes:
#   0 — all scenarios PASS (or SKIP-clean when backend absent)
#   1 — at least one scenario FAIL
#
# Scope: real-case surface coverage for BIND-PY-01..04, BIND-TS-01..04,
# RCPCONV-01 (ChatAgentMemory), RCPDOC-01 (DocumentKnowledgeBase),
# and the Consolidator/Graph pipeline toggles. Results feed into
# `milestones/v0.1.1-EVIDENCE.md` under the SDK-real section.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${REPO_ROOT}/milestones/v0.1.1-sdk-real"
mkdir -p "${OUT_DIR}"

MOON_URL="${LUNARIS_MOON_URL:-moon://127.0.0.1:6380}"
PG_URL="${LUNARIS_TEST_POSTGRES_URL:-}"

export LUNARIS_MOON_URL="${MOON_URL}"
export LUNARIS_TEST_POSTGRES_URL="${PG_URL}"
export LUNARIS_SDK_EVIDENCE_DIR="${OUT_DIR}"

echo "=== Lunaris SDK real-case evidence ==="
echo "Moon:      ${MOON_URL}"
echo "Postgres:  ${PG_URL:-<absent>}"
echo "Out dir:   ${OUT_DIR}"
echo

overall_rc=0

# ---------- Python ----------
# Delegate to the existing pytest suite (which handles pyo3-async-runtimes
# loop lifecycle correctly) plus a small standalone offline runner for the
# DSL-surface smoke. Together they produce real-case evidence for
# BIND-PY-01..04 + RCPCONV-01 + RCPDOC-01 + pipeline toggles.
echo ">>> Python (lunaris-py) — offline surface"
if (cd "${REPO_ROOT}/crates/lunaris-py" && \
    uv run --with "pytest>=8" --with "pytest-asyncio>=0.23" --with "python-ulid" \
      python "${REPO_ROOT}/scripts/sdk-real-evidence.py" 2>&1 \
      | tee "${OUT_DIR}/py-run.log"); then
  py_offline_rc=0
else
  py_offline_rc=$?
fi

echo ">>> Python (lunaris-py) — live pytest against Moon"
(cd "${REPO_ROOT}/crates/lunaris-py" && \
  uv run --with "pytest>=8" --with "pytest-asyncio>=0.23" --with "python-ulid" \
    pytest tests/ -v --tb=short \
      --junit-xml="${OUT_DIR}/py-pytest-junit.xml" 2>&1 \
    | tee "${OUT_DIR}/py-pytest.log") || true

# Synthesize a PASS/FAIL envelope per pytest file from the JUnit XML.
python3 "${REPO_ROOT}/scripts/sdk-real-evidence-pytest-envelope.py" \
  "${OUT_DIR}/py-pytest-junit.xml" "${OUT_DIR}" "${MOON_URL}" \
  || py_live_rc=$?
py_live_rc=${py_live_rc:-0}

echo "python offline_rc=${py_offline_rc} live_rc=${py_live_rc}"
[[ "${py_offline_rc}" -ne 0 ]] && overall_rc=1
[[ "${py_live_rc}" -ne 0 ]] && overall_rc=1

echo

# ---------- TypeScript ----------
echo ">>> TypeScript (lunaris-ts)"
if (cd "${REPO_ROOT}/crates/lunaris-ts" && \
    node --experimental-vm-modules "${REPO_ROOT}/scripts/sdk-real-evidence.mjs" 2>&1 \
      | tee "${OUT_DIR}/ts-run.log"); then
  ts_rc=0
else
  ts_rc=$?
fi
echo "ts runner exit=${ts_rc}"
[[ "${ts_rc}" -ne 0 ]] && overall_rc=1

echo
echo "=== Summary ==="
ls -1 "${OUT_DIR}"/*.json 2>/dev/null || echo "(no envelopes written)"
echo "overall_rc=${overall_rc}"
exit "${overall_rc}"
