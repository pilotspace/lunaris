#!/usr/bin/env bash
# Fail if any example still carries a LIVE instruction for a backend that
# Lunaris 0.7.0 deleted.
#
# Why a grep and not a compiler: `cargo check` / `tsc --noEmit` / `mypy` catch
# API drift, but nothing type-checks a URL string or a shell block in a README.
# The v0.6-era examples type-checked fine while telling every new user to run
# `sqlx migrate` against a Postgres image that no longer exists. That is the
# defect class this guard detects, and neither a compiler nor a type-checker
# can see it.
#
# Historical mentions are allowed ONLY on a line that also carries an explicit
# marker: a pre-0.7 version string, `removed`, `deleted`, `migration`, or the
# escape `no-dead-backends:allow`. Anything else reads as a live instruction.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
self="$(basename "${BASH_SOURCE[0]}")"

# Patterns that name a deleted backend or its tooling.
patterns=(
  'postgres://'
  'postgresql://'
  'sqlite://'
  'LUNARIS_PG_URL'
  'pg-lunaris'
  'pgvector'
  'sqlx migrate'
  'lunaris-storage-postgres'
  'lunaris-storage-sqlite'
)

# A line is exempt when it is visibly historical.
exempt='0\.7\.0|0\.6\.x|0\.6\.2|removed|deleted|migration|no-dead-backends:allow'

status=0
for p in "${patterns[@]}"; do
  hits="$(grep -rn --binary-files=without-match \
            --exclude-dir=target --exclude-dir=node_modules --exclude-dir=.git \
            --exclude='Cargo.lock' --exclude='package-lock.json' \
            --exclude="$self" \
            -F "$p" "$root" || true)"
  [ -z "$hits" ] && continue
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    if printf '%s' "$line" | grep -Eq "$exempt"; then
      continue
    fi
    printf 'dead-backend reference: %s\n' "$line"
    status=1
  done <<< "$hits"
done

if [ "$status" -ne 0 ]; then
  echo ""
  echo "FAIL: examples/ still instructs readers to use a backend deleted in 0.7.0."
  echo "Moon is the only backend. See docs/migration/0.6-to-0.7.md."
  exit 1
fi

echo "OK: no live Postgres/SQLite instructions under examples/."
