#!/usr/bin/env bash
# scripts/bump-version.sh — atomic workspace version bump (RELEASE-04)
#
# Usage: scripts/bump-version.sh <semver>
# Example: scripts/bump-version.sh 0.1.1
#
# Updates:
#   1. All Rust crate versions in the workspace via `cargo set-version --workspace`
#      (cargo-edit plugin; run `cargo install cargo-edit` if missing).
#   2. crates/lunaris-py/pyproject.toml [project].version (line-scoped sed).
#   3. crates/lunaris-ts/package.json .version (jq).
#   4. crates/lunaris-mcp-npm/package.json .version (jq).
#
# After editing, asserts all four surfaces match the requested semver.
# Exits non-zero on any parity mismatch.
#
# Version source-of-truth: root Cargo.toml [workspace.package].version.
# Extract: grep -A 20 '\[workspace.package\]' Cargo.toml | grep '^version' | head -1
# Phase 26 Plan 26-01: added crates/lunaris-mcp-npm/package.json surface.
# Phase 26 Plan 26-02 will add crates/lunaris-mcp-py/pyproject.toml surface.

set -euo pipefail

VER="${1:-}"
if [[ -z "$VER" ]]; then
  echo "Usage: $0 <semver>" >&2
  exit 2
fi

# Validate semver shape (major.minor.patch with optional -pre)
if [[ ! "$VER" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.-]+)?$ ]]; then
  echo "ERROR: '$VER' is not a valid semver" >&2
  exit 3
fi

# Prereq tools
if ! command -v jq >/dev/null 2>&1; then
  echo "ERROR: jq not installed" >&2
  exit 127
fi
if ! cargo set-version --help >/dev/null 2>&1; then
  echo "ERROR: cargo set-version not available. Install cargo-edit with: cargo install cargo-edit" >&2
  exit 127
fi

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

echo "-> Bumping Rust workspace to $VER (cargo set-version --workspace $VER)"
cargo set-version --workspace "$VER"

echo "-> Bumping crates/lunaris-py/pyproject.toml [project].version to $VER"
pyproject="crates/lunaris-py/pyproject.toml"
# Tolerate aligned assignments (`version   = "..."`): match any run of
# whitespace around `=`. Under `pipefail` an unmatched grep would kill the
# whole script silently at the assignment, so `|| true` keeps the explicit
# empty-check below as the real error path.
line=$(grep -nE '^version[[:space:]]*=[[:space:]]*"' "$pyproject" | head -1 | cut -d: -f1 || true)
if [[ -z "$line" ]]; then
  echo "ERROR: could not find version line in $pyproject" >&2
  exit 4
fi
sed -i.bak -E "${line}s/^(version[[:space:]]*=[[:space:]]*)\".*\"/\\1\"$VER\"/" "$pyproject"
rm -f "$pyproject.bak"

echo "-> Bumping crates/lunaris-ts/package.json .version to $VER"
pkgjson="crates/lunaris-ts/package.json"
tmp=$(mktemp)
# .optionalDependencies MUST move with .version. CI publishes the five
# @pilotspace/lunaris-<platform> packages at .version, so a stale pin makes
# the main package request a version that was never published — and because
# napi platform packages are OPTIONAL, npm swallows the 404 and installs a
# binary-less package that only fails at require() time. That is exactly how
# @pilotspace/lunaris@0.5.0 shipped with all five platform packages missing
# (they exist only at 0.3.0). Guarded by
# scripts/tests/test_npm_version_parity.py.
jq --arg v "$VER" \
  '.version = $v
   | .optionalDependencies |= with_entries(.value = $v)' \
  "$pkgjson" > "$tmp"
mv "$tmp" "$pkgjson"

echo "-> Bumping crates/lunaris-mcp-npm/package.json .version to $VER"
mcppkgjson="crates/lunaris-mcp-npm/package.json"
tmp=$(mktemp)
jq --arg v "$VER" '.version = $v' "$mcppkgjson" > "$tmp"
mv "$tmp" "$mcppkgjson"

echo "-> Bumping crates/lunaris-mcp-py/pyproject.toml [project].version to $VER"
py_mcp_pyproject="crates/lunaris-mcp-py/pyproject.toml"
py_mcp_line=$(grep -nE '^version[[:space:]]*=[[:space:]]*"' "$py_mcp_pyproject" | head -1 | cut -d: -f1 || true)
if [[ -z "$py_mcp_line" ]]; then
  echo "ERROR: could not find version line in $py_mcp_pyproject" >&2
  exit 4
fi
sed -i.bak -E "${py_mcp_line}s/^(version[[:space:]]*=[[:space:]]*)\".*\"/\\1\"$VER\"/" "$py_mcp_pyproject"
rm -f "$py_mcp_pyproject.bak"

# integrations/pyproject.toml — the pure-Python `lunaris-integrations`
# adapters. Added 2026-08-26 (W4.7): it was NOT on this list, so it sat at
# 0.7.0 through the whole 0.7.1 release while every other surface moved.
# `integrations-publish.yml` uploads it on a `v*` tag, so a stale number here
# publishes the previous release's version to PyPI. Guarded by
# scripts/tests/test_published_distributions.py.
echo "-> Bumping integrations/pyproject.toml [project].version to $VER"
integrations_pyproject="integrations/pyproject.toml"
integrations_line=$(grep -nE '^version[[:space:]]*=[[:space:]]*"' "$integrations_pyproject" | head -1 | cut -d: -f1 || true)
if [[ -z "$integrations_line" ]]; then
  echo "ERROR: could not find version line in $integrations_pyproject" >&2
  exit 4
fi
sed -i.bak -E "${integrations_line}s/^(version[[:space:]]*=[[:space:]]*)\".*\"/\\1\"$VER\"/" "$integrations_pyproject"
rm -f "$integrations_pyproject.bak"

echo "-> Bumping crates/lunaris-mcp-py/python/lunaris_mcp/__init__.py __version__ to $VER"
py_mcp_init="crates/lunaris-mcp-py/python/lunaris_mcp/__init__.py"
sed -i.bak "s/__version__ = \".*\"/__version__ = \"$VER\"/" "$py_mcp_init"
rm -f "$py_mcp_init.bak"

# 6. crates/lunaris-ts/package-lock.json — the root package entry AND its
#    optionalDependencies pins.
#
#    This step exists because its absence has now broken TWO releases. `npm ci`
#    refuses any lock that disagrees with package.json, so a stale lock takes
#    down every workflow that installs lunaris-ts (v0.7.0: conformance-bindings
#    failed nightly, fixed reactively in 3a1ebb1 "resync the lunaris-ts
#    package-lock so npm ci stops failing EUSAGE").
#
#    `npm install --package-lock-only` CANNOT do this at bump time: the five
#    @pilotspace/lunaris-* platform packages do not exist at the new version
#    until the release publishes them, so npm cannot resolve them and reports
#    "up to date" while leaving the lock stale. Those pins are self-referential
#    — the package's own platform builds — so editing them directly is exact,
#    not a guess.
echo "-> Bumping crates/lunaris-ts/package-lock.json to $VER"
lockjson="crates/lunaris-ts/package-lock.json"
if [[ ! -f "$lockjson" ]]; then
  echo "ERROR: $lockjson not found" >&2
  exit 4
fi
#    The node_modules/@pilotspace/lunaris-* entries carry `resolved` URLs and
#    sha512 `integrity` hashes of the PUBLISHED tarball, which cannot exist for
#    a version that has not shipped. We bump their version and DROP those two
#    fields: npm re-resolves an optional dependency it has no integrity for,
#    and skips it when the platform tarball is unavailable. Verified on 0.7.1 —
#    the parity guard passes and `npm ci` exits 0 (113 packages).
#
#    After the release publishes them, run scripts/restore-ts-lock-integrity.sh
#    to put the hashes back. That is hygiene, not a blocker.
#
#    This line used to say "run `npm install --package-lock-only`". DO NOT.
#    Measured against the real v0.7.1 publish: it printed "found 0
#    vulnerabilities", exited 0, and left the file BYTE-IDENTICAL. The lock
#    already names 0.7.1 for all five platform packages, so npm considers the
#    tree satisfied and does no resolution at all; --prefer-online does not
#    help, because there is no request to make. The repair only works if the
#    five entries are DELETED first, which is what the script does — and then
#    it asserts both fields came back rather than trusting npm's exit code.
tmp_lock=$(mktemp)
jq --arg v "$VER" '
  .version = $v
  | .packages[""].version = $v
  | if (.packages[""].optionalDependencies // {}) | length > 0
    then .packages[""].optionalDependencies |= with_entries(.value = $v)
    else . end
  | .packages |= with_entries(
      if (.key | startswith("node_modules/@pilotspace/lunaris-"))
      then .value = (.value | .version = $v | del(.resolved) | del(.integrity))
      else . end
    )
' "$lockjson" > "$tmp_lock"
mv "$tmp_lock" "$lockjson"

# 7. examples/*/Cargo.lock — each example is its OWN cargo workspace with its
#    own lockfile pinning the lunaris-* path deps. examples.yml regenerates
#    them and fails if the result differs from what is committed ("Cargo.lock
#    is stale"), so a bump that skips them turns the Examples board red.
#
#    Same class of miss as the TS lockfile above, in a third place: the version
#    lives in more surfaces than the ones a bump obviously touches.
for ex_manifest in examples/*/Cargo.toml; do
  [[ -f "$ex_manifest" ]] || continue
  ex_dir=$(dirname "$ex_manifest")
  [[ -f "$ex_dir/Cargo.lock" ]] || continue
  echo "-> Regenerating $ex_dir/Cargo.lock"
  cargo generate-lockfile --manifest-path "$ex_manifest" >/dev/null 2>&1 || {
    echo "ERROR: could not regenerate $ex_dir/Cargo.lock" >&2
    exit 4
  }
done

# 8. crates/lunaris-ts/index.js — napi's generated loader hardcodes the version
#    in its per-platform binding check, 26 times:
#
#      if (bindingPackageVersion !== '0.7.1' && process.env.NAPI_RS_ENFORCE_VERSION_CHECK …)
#        throw new Error(`Native binding package version mismatch, expected 0.7.1 …`)
#
#    `napi build` REGENERATES this file, so it is normally correct by accident —
#    but a bump does not run napi, and nothing else here touched it. Ship 0.7.2
#    with a 0.7.1 loader and every install with NAPI_RS_ENFORCE_VERSION_CHECK
#    set throws at require() time, naming a version nobody published.
#
#    Fourth instance of the same class as the TS lockfile and the example
#    lockfiles above: the version lives in more surfaces than the ones a bump
#    obviously touches. Note this edits `index.js`, NOT the hand-written
#    `index.mjs` shim — `index.mjs` carries no version at all.
indexjs="crates/lunaris-ts/index.js"
if [[ -f "$indexjs" ]]; then
  echo "-> Bumping $indexjs binding-version checks to $VER"
  # Match only a full x.y.z inside quotes/backticks, so a coincidental
  # substring elsewhere in the loader cannot be rewritten.
  perl -pi -e "s/(?<=')[0-9]+\.[0-9]+\.[0-9]+(?=')/$VER/g; s/expected [0-9]+\.[0-9]+\.[0-9]+/expected $VER/g" "$indexjs"
else
  echo "ERROR: $indexjs is missing — napi's generated loader is a version surface" >&2
  exit 4
fi

echo "-> Version parity assertion"
# Rust source-of-truth: root Cargo.toml [workspace.package].version.
rust_ver=$(grep -A 20 '\[workspace.package\]' Cargo.toml | grep '^version' | head -1 | sed 's/version *= *"\(.*\)".*/\1/')
py_ver=$(grep -E '^version[[:space:]]*=[[:space:]]*"' "$pyproject" | head -1 | sed 's/.*"\(.*\)".*/\1/')
ts_ver=$(jq -r '.version' "$pkgjson")
# Any optionalDependency left off $VER would silently 404 at install time.
ts_optdep_stale=$(jq -r --arg v "$VER" \
  '[.optionalDependencies // {} | to_entries[] | select(.value != $v)
    | "\(.key)@\(.value)"] | join(", ")' "$pkgjson")
npm_mcp_ver=$(jq -r '.version' "$mcppkgjson")
lock_ver=$(jq -r '.version' "$lockjson")
lock_root_ver=$(jq -r '.packages[""].version' "$lockjson")
lock_optdep_stale=$(jq -r --arg v "$VER" \
  '[.packages[""].optionalDependencies // {} | to_entries[] | select(.value != $v)
    | "\(.key)@\(.value)"] | join(", ")' "$lockjson")
# The resolution entries too: scripts/tests/test_npm_version_parity.py reads
# THESE, not the optionalDependencies block, and treats an absent entry as a
# failure just like a stale one.
lock_entry_stale=$(jq -r --arg v "$VER" \
  '[.packages | to_entries[]
    | select(.key | startswith("node_modules/@pilotspace/lunaris-"))
    | select(.value.version != $v) | "\(.key)@\(.value.version)"] | join(", ")' "$lockjson")
py_mcp_ver=$(grep -E '^version[[:space:]]*=[[:space:]]*"' "$py_mcp_pyproject" | head -1 | sed 's/.*"\(.*\)".*/\1/')

echo "  Rust (workspace.package): $rust_ver"
echo "  Python (pyproject):       $py_ver"
echo "  TypeScript (package):     $ts_ver"
echo "  npm @pilotspace/lunaris-mcp:         $npm_mcp_ver"
echo "  lunaris-mcp-py (pyproject): $py_mcp_ver"
echo "  TS package-lock:          $lock_ver / $lock_root_ver"

if [[ "$rust_ver" != "$VER" || "$py_ver" != "$VER" || "$ts_ver" != "$VER" || \
      "$npm_mcp_ver" != "$VER" || "$py_mcp_ver" != "$VER" ]]; then
  echo "ERROR: version parity broken" >&2
  exit 5
fi

if [[ -n "$ts_optdep_stale" ]]; then
  echo "ERROR: crates/lunaris-ts/package.json optionalDependencies not at $VER: $ts_optdep_stale" >&2
  exit 5
fi

if [[ "$lock_ver" != "$VER" || "$lock_root_ver" != "$VER" ]]; then
  echo "ERROR: $lockjson not at $VER (top-level: $lock_ver, packages[\"\"]: $lock_root_ver)." >&2
  echo "       npm ci refuses a lock that disagrees with package.json (EUSAGE)." >&2
  exit 5
fi

if [[ -n "$lock_optdep_stale" ]]; then
  echo "ERROR: $lockjson optionalDependencies not at $VER: $lock_optdep_stale" >&2
  exit 5
fi

if [[ -n "$lock_entry_stale" ]]; then
  echo "ERROR: $lockjson resolution entries not at $VER: $lock_entry_stale" >&2
  echo "       test_npm_version_parity.py reads these; npm ci fails EUSAGE." >&2
  exit 5
fi

# The examples pin the workspace crates by path+version; a stale lock here is
# what examples.yml reports as "Cargo.lock is stale".
for ex_lock in examples/*/Cargo.lock; do
  [[ -f "$ex_lock" ]] || continue
  ex_name=$(basename "$(dirname "$ex_lock")")
  if grep -q 'name = "lunaris-memory"' "$ex_lock" && \
     ! grep -A1 'name = "lunaris-memory"' "$ex_lock" | grep -q "version = \"$VER\""; then
    echo "ERROR: $ex_lock still pins lunaris-memory below $VER ($ex_name)" >&2
    exit 5
  fi
done

# napi's loader: every hardcoded binding-version string must be at $VER, and
# there must still BE some — a regex that silently matched nothing would leave
# the file stale and this check green.
indexjs_hits=$(grep -c "'$VER'" "$indexjs" || true)
# BOTH greps need `|| true` under `set -euo pipefail`. `grep -v` exits 1 when it
# filters everything out, which is the SUCCESS case here; and `grep -oE` exits 1
# when the file carries no version strings at all — with `pipefail` that kills
# the script BEFORE the check below can report it, turning the one mutation this
# check exists to catch into a bare `exit 1` with no message.
indexjs_stale=$(
  { grep -oE "'[0-9]+\.[0-9]+\.[0-9]+'" "$indexjs" || true; } \
    | { grep -v "'$VER'" || true; } | sort -u | tr '\n' ' '
)
if [[ "$indexjs_hits" -lt 1 ]]; then
  echo "ERROR: $indexjs has no '$VER' binding-version string — the rewrite matched nothing" >&2
  exit 5
fi
if [[ -n "$indexjs_stale" ]]; then
  echo "ERROR: $indexjs still carries stale binding versions: $indexjs_stale" >&2
  exit 5
fi
echo "  TS index.js binding checks: $indexjs_hits at $VER"

echo "OK: all five surfaces at $VER (and the TS package-lock + example locks)"
echo "Next: cargo build --workspace --all-targets --all-features; then follow 13-03-HUMAN-UAT.md"
