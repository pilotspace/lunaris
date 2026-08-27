#!/usr/bin/env bash
# Assert that strict mode ACTUALLY FIRES — ship-plan F36.
#
# The existing guards are each necessary and together insufficient:
#
#   * F32's guard proves `LUNARIS_CONFORMANCE_STRICT` is parsed the same way
#     everywhere.
#   * The routing sweep proves each suite sends its skip through the helper.
#
# Neither runs the ONE arm that discriminates a working strict mode from a
# decorative one: strict ON with the fixture ABSENT. `integration.yml` cannot
# run it, because that job sets `LUNARIS_CONFORMANCE_STRICT=1` *and* builds a
# Moon — so strict never fires there. A suite whose helper call is unreachable
# is simultaneously green in CI, green in the sweep, and green in the flag
# guard.
#
# This script runs that arm. Every target listed below MUST FAIL; a target that
# passes with no fixture is one whose strict routing does not execute.
#
# Cost: near zero when run after the integration job's own steps, because every
# test binary is already compiled — this only re-runs them with different env.
#
# Local use (from the repo root):
#     bash scripts/ci/assert-strict-fires.sh
set -uo pipefail

# The fixture must be absent for every backend the suites can reach.
unset MOON_URL LUNARIS_MOON_URL PG_URL DATABASE_URL
export LUNARIS_CONFORMANCE_STRICT=1
export MOON_TEST_BINARY=/nonexistent

fail=0
log_dir="${RUNNER_TEMP:-/tmp}/strict-fires"
mkdir -p "$log_dir"

# must_fail <label> <cargo args...>
must_fail() {
  local label="$1"; shift
  if "$@" >"$log_dir/$label.log" 2>&1; then
    echo "::error::$label PASSED with strict ON and no fixture — strict mode is DECORATIVE for this target. Its skip path is unreachable, so it is green here, in the routing sweep, and in the flag guard, all at once."
    tail -20 "$log_dir/$label.log" || true
    fail=1
  else
    echo "ok — $label failed as required"
  fi
}

must_fail conformance    cargo test -p lunaris-conformance  --features moon-it --no-fail-fast
must_fail ingest         cargo test -p lunaris-ingest       --features moon-it --no-fail-fast
must_fail storage-moon   cargo test -p lunaris-storage-moon --features moon-it --no-fail-fast
must_fail server-graph   cargo test -p lunaris-server   --test recall_graph_mode_live --no-fail-fast
must_fail retrieve-aggr  cargo test -p lunaris-retrieve --test d_aggregate_operator  --no-fail-fast
# R5 (2026-08-27): integration.yml now runs `-p lunaris-retrieve` against the
# live Moon. This entry is what stops that step from silently degrading into a
# no-op — `navigate_filter_moon` routes its skip through
# `strict_skip::note_unavailable`, so if the fixture ever stops being reachable
# the suite must go RED, not report a 0.00s green.
must_fail retrieve-nav   cargo test -p lunaris-retrieve --test navigate_filter_moon --no-fail-fast

# The lunaris-memory targets integration.yml un-ignores, each checked
# separately: a single cargo invocation would let ONE failing target mask a
# sibling whose strict routing is unreachable.
for t in moon_parity coding_session_memory_smoke consolidator_scope_isolation \
         phase_14_1_reflect_invalidate chaos_helios_sigkill \
         as_of_scratchpad_read as_of_scratchpad_content \
         audit_scope_isolation retention_policy; do
  must_fail "memory-$t" cargo test -p lunaris-memory --test "$t" --no-fail-fast -- --include-ignored
done

# DELIBERATELY NOT CHECKED: -p lunaris-recipes.
#
# It has no live-fixture tests for strict to fire on. All three of its
# integration files define their OWN mock `StoragePort` in-file, so they pass
# with no Moon by design, and the crate contains zero `#[cfg(feature =
# "moon-it")]` sites — the feature only pulls an optional dependency. Listing
# it here would make this gate fail forever on a target that is behaving
# correctly.
#
# integration.yml used to run `-p lunaris-recipes --features moon-it` inside the
# Moon-having job, with a dedicated fresh-Moon reset before it, against a
# package that touches no Moon. That step and the vacuous feature are both
# gone (F36); this comment stays so the next reader knows the omission below
# is a decision, not an oversight.

if [ "$fail" -ne 0 ]; then
  echo "::error::strict mode is decorative for at least one target (above)"
  exit 1
fi
echo "all targets failed as required — strict mode fires"
