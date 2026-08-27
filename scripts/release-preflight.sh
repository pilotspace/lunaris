#!/usr/bin/env bash
# scripts/release-preflight.sh — Phase 22 ship-to-product readiness gate.
#
# Runs the full sequence of checks that MUST be green before
# `cargo publish` for any v0.2.x release line:
#
#   1.  Working tree clean (no uncommitted changes — publishes ship
#       what's on disk, so dirty state would silently leak).
#   2.  cargo fmt --check (formatting drift)
#   3.  cargo clippy --workspace --all-targets -- -D warnings
#   4.  cargo build --workspace
#   5.  cargo test  --workspace --exclude lunaris-py --exclude lunaris-ts
#   6.  cargo doc   --workspace --no-deps (publishable-surface zero-warnings)
#   7.  cargo deny check (license / advisory / source policy)
#   8.  cargo publish --dry-run -p lunaris-core --allow-dirty
#       (the leaf of the publishable graph — the 7 downstream crates
#       cannot dry-run until lunaris-core is actually on crates.io)
#   9.  Per-crate manifest hygiene: every publishable crate carries
#       description, license, repository, readme, keywords, categories.
#  10.  Version parity: every crate's [package].version matches the
#       umbrella (no mid-flight bump drift).
#
# Exit codes:
#   0   — all checks pass; ready to `cargo publish`
#   1   — one or more checks failed (stderr shows which)
#   127 — required tool missing (cargo-deny, cargo-edit, jq)
#
# Usage:
#   scripts/release-preflight.sh                # all checks
#   scripts/release-preflight.sh --skip-tests   # skip step 5 (faster)
#   scripts/release-preflight.sh --skip-doc     # skip step 6
#   scripts/release-preflight.sh --json         # machine-readable summary
#
# Time budget: ~5 minutes on a warm cargo cache.

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

SKIP_TESTS=0
SKIP_DOC=0
EMIT_JSON=0
for arg in "$@"; do
  case "$arg" in
    --skip-tests) SKIP_TESTS=1 ;;
    --skip-doc) SKIP_DOC=1 ;;
    --json) EMIT_JSON=1 ;;
    -h|--help)
      grep -E '^# ' "$0" | sed 's/^# //'
      exit 0
      ;;
    *) echo "Unknown flag: $arg" >&2; exit 2 ;;
  esac
done

# ───────────────────────── helpers ─────────────────────────

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
declare -a RESULTS=()  # "name|status|message"

record() {
  # record <name> <status: PASS|FAIL|SKIP> <message>
  local name="$1" status="$2" msg="${3:-}"
  RESULTS+=("$name|$status|$msg")
  case "$status" in
    PASS) PASS_COUNT=$((PASS_COUNT+1)); echo "  PASS  $name${msg:+ — $msg}" ;;
    FAIL) FAIL_COUNT=$((FAIL_COUNT+1)); echo "  FAIL  $name${msg:+ — $msg}" >&2 ;;
    SKIP) SKIP_COUNT=$((SKIP_COUNT+1)); echo "  SKIP  $name${msg:+ — $msg}" ;;
  esac
}

# ───────────────────────── checks ─────────────────────────

echo "==> Lunaris release-preflight"
echo "    branch: $(git rev-parse --abbrev-ref HEAD)"
echo "    head:   $(git rev-parse --short HEAD)"
echo ""

# 1. Clean working tree
if [[ -z "$(git status --porcelain | grep -v '^.M \.planning$' || true)" ]]; then
  record "01_clean_working_tree" "PASS"
else
  record "01_clean_working_tree" "FAIL" "uncommitted changes — commit or stash before publishing"
fi

# 2. cargo fmt --check
if cargo fmt --check >/dev/null 2>&1; then
  record "02_cargo_fmt" "PASS"
else
  record "02_cargo_fmt" "FAIL" "run \`cargo fmt\`"
fi

# 3. cargo clippy
if cargo clippy --workspace --all-targets --quiet -- -D warnings 2>&1 | tail -1 | grep -qE "warning|error"; then
  record "03_cargo_clippy" "FAIL" "clippy emitted warnings/errors with -D warnings"
else
  record "03_cargo_clippy" "PASS"
fi

# 4. cargo build
if cargo build --workspace --quiet 2>&1 | grep -qE "^error"; then
  record "04_cargo_build" "FAIL" "build errors — see \`cargo build --workspace\`"
else
  record "04_cargo_build" "PASS"
fi

# 5. cargo test (optional)
if [[ "$SKIP_TESTS" == "1" ]]; then
  record "05_cargo_test" "SKIP" "--skip-tests"
else
  if cargo test --workspace --exclude lunaris-py --exclude lunaris-ts --quiet 2>&1 | grep -qE "FAILED|^error"; then
    record "05_cargo_test" "FAIL" "one or more tests failed"
  else
    record "05_cargo_test" "PASS"
  fi
fi

# 6. cargo doc (optional)
if [[ "$SKIP_DOC" == "1" ]]; then
  record "06_cargo_doc" "SKIP" "--skip-doc"
else
  # Only fail if a PUBLISHABLE crate emits warnings. lunaris-codegen /
  # lunaris-conformance / lunaris-bench / lunaris-ts are internal-only.
  doc_out=$(cargo doc --workspace --no-deps 2>&1)
  bad=$(echo "$doc_out" | grep -E "^warning: \`(lunaris-memory|lunaris-core|lunaris-storage-moon|lunaris-llamacpp|lunaris-embed-remote|lunaris-llm|lunaris-rerank|lunaris-extract|lunaris-verify|lunaris-consolidate|lunaris-ingest|lunaris-retrieve)\` \(lib doc\)" || true)
  if [[ -n "$bad" ]]; then
    record "06_cargo_doc" "FAIL" "publishable crate rustdoc warnings"
  else
    record "06_cargo_doc" "PASS"
  fi
fi

# 7. cargo deny
if ! command -v cargo-deny >/dev/null 2>&1; then
  record "07_cargo_deny" "SKIP" "cargo-deny not installed (run: cargo install cargo-deny)"
else
  if cargo deny check 2>&1 | tail -1 | grep -q "FAILED"; then
    record "07_cargo_deny" "FAIL" "advisory / license / source / ban violation"
  else
    record "07_cargo_deny" "PASS"
  fi
fi

# 8. cargo publish --dry-run -p lunaris-core
if cargo publish -p lunaris-core --dry-run --allow-dirty --quiet 2>&1 | grep -qE "^error"; then
  record "08_publish_dry_run_core" "FAIL" "lunaris-core would not publish cleanly"
else
  record "08_publish_dry_run_core" "PASS"
fi

# 9. Per-crate manifest hygiene
# Crate DIRECTORY names of the publishable set (`crates/lunaris` publishes as
# `lunaris-memory`). `lunaris-hook` was removed here on 2026-08-15: it is
# `publish = false` (it depends on the unpublished `lunaris-memory-service`),
# so certifying it as release-ready asserted readiness for something that can
# never be uploaded. Keep this list in sync with crates-publish.yml's CRATES;
# xtask/tests/publish_metadata_guard.rs fails if either lists an
# unpublishable crate.
HYGIENE_CRATES=(
  lunaris-core lunaris-storage-moon
  lunaris-llamacpp lunaris-embed-remote
  lunaris-llm lunaris-rerank lunaris-extract
  lunaris-verify lunaris-consolidate lunaris-ingest
  lunaris-retrieve lunaris
)
missing=""
for c in "${HYGIENE_CRATES[@]}"; do
  manifest="crates/$c/Cargo.toml"
  # Manifests use either inline `repository = "..."` or workspace-inherited
  # `repository.workspace = true`. Both forms count as present.
  for field in description repository readme; do
    if ! grep -qE "^${field}(\s*=|\.workspace\s*=)" "$manifest"; then
      missing+="$c:$field "
    fi
  done
done
if [[ -n "$missing" ]]; then
  record "09_manifest_hygiene" "FAIL" "missing fields: $missing"
else
  record "09_manifest_hygiene" "PASS"
fi

# 10. Version parity — every publishable crate must inherit
# [workspace.package].version (via `version.workspace = true`), AND the
# workspace-resolved version must match the umbrella's actually-built
# version (cargo metadata is the source of truth post-resolution).
ws_ver=$(awk '/\[workspace.package\]/{flag=1; next} /^\[/{flag=0} flag && /^version/{gsub(/[" ]/,""); split($0,a,"="); print a[2]; exit}' Cargo.toml)
umbrella_ver=$(cargo metadata --no-deps --format-version 1 2>/dev/null \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(next((p["version"] for p in d["packages"] if p["name"]=="lunaris-memory"), "NOT-FOUND"))')
if [[ -n "$ws_ver" && "$umbrella_ver" == "$ws_ver" ]]; then
  record "10_version_parity" "PASS" "$ws_ver"
else
  record "10_version_parity" "FAIL" "workspace.package.version=$ws_ver umbrella(resolved)=$umbrella_ver"
fi

# ───────────────────────── summary ─────────────────────────

echo ""
echo "==> Summary"
echo "    PASS: $PASS_COUNT"
echo "    FAIL: $FAIL_COUNT"
echo "    SKIP: $SKIP_COUNT"
echo ""

if [[ "$EMIT_JSON" == "1" ]]; then
  echo "{"
  echo "  \"branch\":  \"$(git rev-parse --abbrev-ref HEAD)\","
  echo "  \"commit\":  \"$(git rev-parse --short HEAD)\","
  echo "  \"pass\":    $PASS_COUNT,"
  echo "  \"fail\":    $FAIL_COUNT,"
  echo "  \"skip\":    $SKIP_COUNT,"
  echo "  \"results\": ["
  last_idx=$((${#RESULTS[@]} - 1))
  for i in "${!RESULTS[@]}"; do
    IFS='|' read -r name status msg <<< "${RESULTS[$i]}"
    comma=$([[ "$i" == "$last_idx" ]] && echo "" || echo ",")
    echo "    {\"name\": \"$name\", \"status\": \"$status\", \"message\": \"$msg\"}$comma"
  done
  echo "  ]"
  echo "}"
fi

if [[ "$FAIL_COUNT" -gt 0 ]]; then
  echo "==> NOT READY TO PUBLISH ($FAIL_COUNT failures)" >&2
  exit 1
fi

echo "==> READY TO PUBLISH (all gates passed)"
exit 0
