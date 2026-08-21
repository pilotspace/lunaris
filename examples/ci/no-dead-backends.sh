#!/usr/bin/env bash
# Fail if any example or doc still carries a LIVE instruction for a backend
# that Lunaris 0.7.0 deleted.
#
# Why a grep and not a compiler: `cargo check` / `tsc --noEmit` / `mypy` catch
# API drift, but nothing type-checks a URL string or a shell block in a README.
# The v0.6-era examples type-checked fine while telling every new user to run
# `sqlx migrate` against a Postgres image that no longer exists. That is the
# defect class this guard detects, and neither a compiler nor a type-checker
# can see it.
#
# Scan roots (widened 2026-08-22, F15): the rationale above applies at least as
# strongly to `docs/` and the top-level README, which is where a new user
# actually reads. Scanning `examples/` alone left `docs/guide.md` telling
# readers "Two backends ship Day-0" and handing them a `postgres://` URL to
# pass to `Lunaris::open`, which now returns UnsupportedScheme.
#
# Historical mentions are allowed two ways:
#
#  1. The whole file is a historical record — a migration note, a planning
#     doc, an archived changelog, a superseded RFC. Those trees are listed in
#     `historical_paths` below and skipped wholesale.
#  2. The individual LINE is visibly historical: it carries a pre-0.7 version
#     string, or one of `removed` / `deleted` / `retired` / `migration`, or the
#     explicit escape `no-dead-backends:allow`.
#
# Anything else reads as a live instruction.
#
# Two defects fixed alongside the widening, both of which made the guard weaker
# than it looked:
#
#  - The exempt regex used to be tested against the whole `grep -rn` line,
#    which INCLUDES the path. Every file under `docs/migration/` was therefore
#    auto-exempt by its directory name alone, whatever the line said. The
#    exemption is now tested against the line CONTENT only; the path decides
#    exemption solely through the explicit `historical_paths` list.
#  - `retired` was not in the vocabulary, although `docs/book/src/operations/
#    backends.md` uses exactly that word to describe the deletion correctly.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
self="$(basename "${BASH_SOURCE[0]}")"

# Where a live instruction can reach a reader.
roots=(
  "$repo/examples"
  "$repo/docs"
  "$repo/README.md"
)

# Patterns that name a deleted backend or its tooling.
#
# Every entry is a symbol, scheme, or env var that ONLY Lunaris could own.
# That is deliberate: a bare `Postgres` cannot distinguish "Lunaris supports
# Postgres" (false since 0.7.0) from "Zep runs on Postgres" (true, and the
# whole point of the comparison tables), and a guard that flags both gets
# muzzled with escapes until it detects nothing. Prose claims are a review
# problem; these are a grep problem.
patterns=(
  'postgres://'
  'postgresql://'
  'sqlite://'
  'LUNARIS_PG_URL'
  'PG_URL'
  'pg-lunaris'
  'pgvector'
  'pgmq'
  'sqlx migrate'
  'lunaris-storage-postgres'
  'lunaris-storage-sqlite'
  'PostgresStorage'
  'SqliteStorage'
  'run_storage_postgres'
  'run_storage_sqlite'
  'run_as_of_parity'
)

# Trees whose whole purpose is to record what the project USED to do, or
# what someone else does. A migration note that cannot name the thing it
# migrates from is useless; so is a competitive comparison that cannot name
# the competitor's stack. `design/` and `testing/` are dated records of a
# decision at a point in time, not instructions to follow today.
historical_paths='^docs/(migration|planning|rfcs|spikes|design|testing|competitive|release/deprecations\.md|CHANGELOG-archive\.md|v0\.3-known-debt\.md|benchmarks/v0\.)'

# A line is exempt when its CONTENT is visibly historical.
exempt='0\.7\.0|0\.6\.x|0\.6\.2|0\.6\.1|0\.5\.|0\.3\.|removed|deleted|retired|migration|no-dead-backends:allow'

status=0
scanned=0
for p in "${patterns[@]}"; do
  hits="$(grep -rn --binary-files=without-match \
            --exclude-dir=target --exclude-dir=node_modules --exclude-dir=.git \
            --exclude='Cargo.lock' --exclude='package-lock.json' \
            --exclude="$self" \
            -F "$p" "${roots[@]}" || true)"
  [ -z "$hits" ] && continue
  while IFS= read -r hit; do
    [ -z "$hit" ] && continue
    scanned=$((scanned + 1))
    # `path:lineno:content` — split so the path can never satisfy the
    # content-level exemption (see the module note above).
    where="${hit%%:*}"
    rest="${hit#*:}"
    lineno="${rest%%:*}"
    content="${rest#*:}"
    rel="${where#"$repo"/}"

    if printf '%s' "$rel" | grep -Eq "$historical_paths"; then
      continue
    fi
    if printf '%s' "$content" | grep -Eq "$exempt"; then
      continue
    fi
    printf 'dead-backend reference: %s:%s: %s\n' "$rel" "$lineno" "$content"
    status=1
  done <<< "$hits"
done

# Vacuity floor. If a refactor moves the docs or renames the roots, this guard
# would otherwise scan nothing and report OK forever — the exact shape it
# exists to catch. The historical trees alone carry far more than 20 mentions.
if [ "$scanned" -lt 20 ]; then
  echo "FAIL: the guard matched only $scanned lines across ${roots[*]}."
  echo "That is below the floor for a repo that documents its own migration"
  echo "away from Postgres — the scan roots are probably wrong."
  exit 1
fi

if [ "$status" -ne 0 ]; then
  echo ""
  echo "FAIL: the docs still instruct readers to use a backend deleted in 0.7.0."
  echo "Moon is the only backend. See docs/migration/0.6-to-0.7.md."
  echo ""
  echo "If a line is genuinely historical, say so ON the line (name the version,"
  echo "or write removed/deleted/retired), or add its tree to historical_paths."
  exit 1
fi

echo "OK: no live Postgres/SQLite instructions under examples/, docs/, README.md ($scanned mentions checked)."
