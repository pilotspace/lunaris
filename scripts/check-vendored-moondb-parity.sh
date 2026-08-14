#!/usr/bin/env bash
#
# Verify that the vendored moondb SDK source matches what is actually on
# crates.io at the version it declares.
#
# WHY THIS EXISTS
# ---------------
# `.github/workflows/crates-publish.yml` skips uploading moondb when its
# version is already on crates.io. A version *string* is not an identity, and
# treating it as one hid a real divergence for two months:
#
#   * the workspace pins `moon = { path = "vendor/moon/sdk/rust",
#     version = "0.2.1", package = "moondb" }`;
#   * the submodule is pinned at moon `v0.8.5`, whose `sdk/rust/Cargo.toml`
#     STILL declares `version = "0.2.1"` — the string uploaded on 2026-06-13;
#   * but the source underneath moved several moon releases forward.
#
# Measured 2026-08-15: 9 of 13 source files differ, and published moondb 0.2.1
# `client.rs` contains ZERO occurrences of `ConnectionManager` (it is still on
# `MultiplexedConnection`) against SEVEN in the pinned source. The reconnect fix
# (moon PR #419) was therefore missing from every crates.io consumer of the
# published lunaris crates, while the job cheerfully logged
# "moondb 0.2.1 already on crates.io — skipping" on every run.
#
# crates.io versions are immutable, so the repair is NOT to republish 0.2.1.
# It is to FAIL LOUDLY so the operator cuts a real moondb release and bumps the
# vendored manifest and the workspace pin together. See
# `docs/release/deprecations.md` §2 for the exact owner procedure.
#
# USAGE
# -----
#   check-vendored-moondb-parity.sh
#       CI mode. Reads the version from vendor/moon/sdk/rust/Cargo.toml, asks
#       crates.io whether it exists, and if so downloads the published .crate
#       and compares its src/ against the vendored src/.
#
#   check-vendored-moondb-parity.sh <VENDORED_SRC_DIR> <PUBLISHED_SRC_DIR>
#       Comparison mode. Compares two already-materialised source trees and
#       performs no network I/O. This is the seam the guard tests drive
#       (xtask/tests/publish_metadata_guard.rs) so the comparison logic is
#       verified hermetically and offline.
#
# EXIT CODES
# ----------
#   0  safe to proceed — either the version is new (nothing published to
#      contradict), or it is published and the source is identical.
#   1  DIVERGENCE — published at this version but the source differs.
#   2  usage / environment error. Deliberately distinct from 1 so that
#      "the guard could not run" is never mistaken for "the guard passed".

set -euo pipefail

CRATE="moondb"
VENDORED_MANIFEST="vendor/moon/sdk/rust/Cargo.toml"
VENDORED_SRC="vendor/moon/sdk/rust/src"
UA="lunaris-release-ci (github.com/pilotspace/lunaris)"

die_usage() {
  echo "error: $1" >&2
  echo "" >&2
  echo "usage: $0                                          # CI mode" >&2
  echo "       $0 <VENDORED_SRC_DIR> <PUBLISHED_SRC_DIR>   # comparison mode" >&2
  exit 2
}

# Compare two source trees. Exits 0 on identical, 1 on divergence.
compare_trees() {
  local vendored="$1" published="$2"
  [ -d "$vendored" ] || die_usage "vendored source dir not found: $vendored"
  [ -d "$published" ] || die_usage "published source dir not found: $published"

  # `set -e` must not abort on diff's expected non-zero exit — the exit status
  # IS the answer here, not a failure.
  local report status
  set +e
  report=$(diff -r -q "$published" "$vendored" 2>&1)
  status=$?
  set -e

  if [ "$status" -eq 0 ]; then
    echo "moondb source parity OK — vendored tree is byte-identical to the published crate."
    return 0
  fi

  echo "::error::vendored moondb source does NOT match the crate published at the same version" >&2
  echo "" >&2
  echo "$report" >&2
  echo "" >&2
  cat >&2 <<'REMEDY'
crates.io versions are immutable, so this cannot be fixed by republishing.

Remedy (owner action, in the `moon` repo — see docs/release/deprecations.md §2):
  1. Bump `sdk/rust/Cargo.toml` in pilotspace/moon to a NEW version. The SDK has
     gained public API since 0.2.1 (ConnectionManager-based reconnect among it),
     so 0.3.0 is the floor — not a patch bump.
  2. `cargo publish --manifest-path sdk/rust/Cargo.toml` from that repo.
  3. Back in this repo, bump BOTH halves of the pin together:
       - the `vendor/moon` submodule -> the new moon tag, and
       - `moon = { ..., version = "<new>", package = "moondb" }` in Cargo.toml.
     The existing "workspace moon pin matches vendored moondb version" step
     fails if those two drift; this check fails if the source drifts.
  4. Re-run crates-publish (workflow_dispatch works without re-tagging).

Publishing lunaris crates while this is red ships manifests pinning a moondb
that is NOT the moondb they were built and tested against. That is the exact
failure mode that blocked the 0.3.0 release.
REMEDY
  return 1
}

is_published() {
  curl -fsS -A "$UA" "https://crates.io/api/v1/crates/$1/$2" >/dev/null 2>&1
}

ci_mode() {
  [ -f "$VENDORED_MANIFEST" ] || die_usage \
    "$VENDORED_MANIFEST not found — run from the repo root with the vendor/moon submodule initialised"

  local version
  version=$(grep -E '^version = ' "$VENDORED_MANIFEST" | head -1 | sed -E 's/version = "([^"]+)"/\1/')
  [ -n "$version" ] || die_usage "could not read a version out of $VENDORED_MANIFEST"

  if ! is_published "$CRATE" "$version"; then
    echo "$CRATE $version is not on crates.io yet — nothing to compare, publish will proceed."
    return 0
  fi

  echo "$CRATE $version is already on crates.io — verifying the vendored source matches it."

  local work
  work=$(mktemp -d)
  # shellcheck disable=SC2064  # expand $work now, not at trap time.
  trap "rm -rf '$work'" EXIT

  if ! curl -fsSL -A "$UA" \
      "https://static.crates.io/crates/$CRATE/$CRATE-$version.crate" \
      -o "$work/$CRATE.crate"; then
    die_usage "failed to download $CRATE-$version.crate from static.crates.io"
  fi
  tar -xzf "$work/$CRATE.crate" -C "$work" \
    || die_usage "failed to extract $CRATE-$version.crate"

  compare_trees "$VENDORED_SRC" "$work/$CRATE-$version/src"
}

case $# in
  0) ci_mode ;;
  2) compare_trees "$1" "$2" ;;
  *) die_usage "expected 0 or 2 arguments, got $#" ;;
esac
