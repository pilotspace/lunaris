#!/usr/bin/env bash
# Local CI — the PR-time merge gate that used to burn 20+ min of GitHub
# Actions per PR (CI diet, 2026-08-18). Runs the important suites at local
# speed; GitHub Actions keeps the lean core (ci.yml, CodeQL, Docs) on PRs
# and the full matrices on main-push / nightly cron / tags.
#
# Usage:
#   scripts/ci/local-ci.sh            # quick tier: fmt, clippy, tests, parity
#   scripts/ci/local-ci.sh full       # + SDK builds/tests, cargo-deny, ratchet
#
# Env:
#   MOON_TEST_BINARY   moon >=0.8.5 binary for Moon-backed suites
#                      (default: vendor/moon/target/release/moon if built)
#   LOCAL_CI_RATCHET=1 include the LME any-gold ratchet in `full`
#                      (needs GGUF models + dataset cache; ~10 min)
#
# Contract (docs/ci/local-ci.md): run `quick` before every push, `full`
# before merging anything that touches SDKs, storage, or retrieval.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

TIER="${1:-quick}"
case "$TIER" in quick|full) ;; *) echo "usage: $0 [quick|full]" >&2; exit 2 ;; esac

if [ -z "${MOON_TEST_BINARY:-}" ] && [ -x "$ROOT/vendor/moon/target/release/moon" ]; then
  export MOON_TEST_BINARY="$ROOT/vendor/moon/target/release/moon"
fi

declare -a NAMES RESULTS TIMES
step() { # $1 = name, rest = command
  local name="$1"; shift
  local t0 t1 rc
  echo
  echo "━━━ $name"
  t0=$(date +%s)
  "$@"; rc=$?
  t1=$(date +%s)
  NAMES+=("$name"); TIMES+=("$((t1 - t0))s")
  if [ $rc -eq 0 ]; then RESULTS+=("PASS"); else RESULTS+=("FAIL"); fi
  return 0
}
skip() { NAMES+=("$1"); RESULTS+=("SKIP"); TIMES+=("-"); echo; echo "━━━ $1 — SKIPPED: $2"; }

# Per-crate fmt (the repo convention forbids `cargo fmt --all`).
fmt_check() {
  local bad=0 d
  for d in crates/*/; do
    [ -f "$d/Cargo.toml" ] || continue
    (cd "$d" && cargo fmt --check >/dev/null 2>&1) || { echo "fmt drift: $d"; bad=1; }
  done
  return $bad
}

step "fmt (per-crate)"          fmt_check
step "clippy --workspace --all-targets" \
  cargo clippy --workspace --all-targets --quiet -- -D warnings
step "cargo test --workspace (sans py/ts cdylibs)" \
  cargo test --workspace --exclude lunaris-py --exclude lunaris-ts --quiet
step "npm version parity"       python3 scripts/tests/test_npm_version_parity.py

if [ "$TIER" = "full" ]; then
  if command -v cargo-deny >/dev/null 2>&1; then
    step "cargo-deny advisories"  cargo deny check advisories
  else
    skip "cargo-deny advisories" "cargo-deny not installed (cargo install cargo-deny)"
  fi

  if command -v maturin >/dev/null 2>&1 && python3 -c 'import pytest' 2>/dev/null; then
    step "lunaris-py (maturin+pytest)" bash -c \
      'cd crates/lunaris-py && maturin develop --release -q && python3 -m pytest -q tests'
  else
    skip "lunaris-py (maturin+pytest)" "maturin or pytest missing"
  fi

  if command -v npm >/dev/null 2>&1 && [ -f crates/lunaris-ts/package.json ]; then
    step "lunaris-ts (napi+vitest)" bash -c \
      'cd crates/lunaris-ts && npm ci --silent && npm run build --silent && npm test --silent'
  else
    skip "lunaris-ts (napi+vitest)" "npm missing"
  fi

  if [ "${LOCAL_CI_RATCHET:-0}" = "1" ]; then
    step "LME any-gold ratchet" scripts/bench/lme/anygold_gate.sh \
      --baseline scripts/bench/lme/baselines/ci-anygold.json
  else
    skip "LME any-gold ratchet" "opt-in: LOCAL_CI_RATCHET=1 (needs models + dataset)"
  fi
fi

echo
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
fail=0
for i in "${!NAMES[@]}"; do
  printf "%-42s %-5s %s\n" "${NAMES[$i]}" "${RESULTS[$i]}" "${TIMES[$i]}"
  [ "${RESULTS[$i]}" = "FAIL" ] && fail=1
done
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
if [ $fail -ne 0 ]; then echo "local-ci: FAIL"; exit 1; fi
echo "local-ci: OK ($TIER tier)"
