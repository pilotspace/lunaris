#!/usr/bin/env bash
# scripts/check-binary-size.sh — N-04 D4 release-binary-size CI gate.
#
# Builds `lunaris-server` in release mode and runs the
# `binary_size_gate` test against the 35 MiB ceiling. Wraps the two
# steps CI invokes in sequence so dev boxes can verify the gate
# locally with one command.
#
# Why a wrapper script?
#
#   The gate is a Rust test (so the failure message names the path +
#   measured size) but the binary must be on disk before the test
#   runs. A test does NOT trigger its own build — Cargo's test
#   harness only builds dev-deps. Hence the shell wrapper.
#
# Exit codes:
#   0   — binary size at or below ceiling
#   1   — build failed OR binary exceeds ceiling
#
# CI invocation: identical to local — no env-var knobs.
#
set -euo pipefail

cd "$(dirname "$0")/.."

echo "[check-binary-size] Building lunaris-server release binary..."
cargo build --release -p lunaris-server

echo "[check-binary-size] Running binary_size_gate test..."
cargo test \
    -p lunaris-bench \
    --features ci-perf-gates \
    --release \
    --test binary_size_gate \
    -- --include-ignored --nocapture
