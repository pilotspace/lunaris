#!/bin/sh
# Install the Moon server that Lunaris requires.
#
#   curl -fsSL https://raw.githubusercontent.com/pilotspace/lunaris/main/scripts/install-moon.sh | sh
#
# WHY THIS EXISTS, AND WHY IT IS NOT MOON'S OWN install.sh
# --------------------------------------------------------
# Moon's installer (`pilotspace/moon:install.sh`) downloads a release tarball
# and, with no VERSION set, resolves to the *latest* tag. As of 2026-08-25 the
# three most recent Moon releases -- v0.8.5, v0.8.6, v0.8.7 -- all publish ZERO
# binary assets (v0.8.4 has 48), and the ghcr image is private. So the
# documented one-liner cannot succeed on any machine, and every Lunaris install
# guide pointed at it. A user with no Moon was told to run a command that 404s.
#
# Lunaris must not be gated on another project's release pipeline. This script
# owns the problem instead: it walks a ladder and only fails if EVERY rung
# fails.
#
#   1. A Moon already on this machine                          -> reuse it.
#   2. A published release tarball for the pinned tag          -> fast path.
#   3. Shallow clone of the public source tag + cargo install  -> works today.
#
# Rung 2 is dead right now and is kept deliberately: the moment Moon re-cuts
# its assets, installs get fast again with no change here. Rung 3 is what
# actually runs -- `pilotspace/moon` is a public Apache-2.0 repo, so the source
# build needs no token and no owner action.
#
# PINNED, NEVER "LATEST". Lunaris refuses at connect any Moon below
# MIN_MOON_VERSION, and "latest" is how you land on a tag with no assets. We
# ask for exactly the version Lunaris was tested against. Set MOON_VERSION= to
# pin forward (validated against the floor); INSTALL_DIR= to choose a target.
#
# THE MOON BINARY HAS NO VERSION FLAG
# -----------------------------------
# `moon --version` and `moon -V` are both rejected ("unexpected argument"), and
# the version is not recoverable from the binary with `strings` -- it is
# assembled at runtime. The ONLY surface is `INFO server | grep moon_version`,
# which needs a RUNNING server. This script must never start one (see PORTS
# below), so it cannot interrogate an arbitrary binary's version offline.
#
# So: we record the version we install in a marker file next to the binary, and
# trust it on later runs. A Moon we did not install has no marker and cannot be
# verified -- we reuse it and say so, rather than rebuilding a perfectly good
# server or refusing to proceed. That is safe because the REAL gate is
# downstream and already excellent: Lunaris checks moon_version at connect and
# fails with a message naming both versions and the fix.
#
# PORTS: none. This script installs a binary and runs `moon --help`. It never
# starts a server, so it cannot touch 6379/6380/6381 or any bench port.
set -eu

MOON_REPO="https://github.com/pilotspace/moon"

# Keep in lockstep with MIN_MOON_VERSION in
# crates/lunaris-storage-moon/src/version.rs. The two are checked against each
# other by scripts/tests/test_moon_install_is_lunaris_owned.py, which fails the
# build on drift -- an installer that fetches a Moon the engine then rejects at
# connect is worse than no installer at all.
MIN_MOON_VERSION="0.8.5"

# Resolved in main() from MOON_VERSION, defaulting to the floor. Operators may
# pin FORWARD (a Moon carrying a fix Lunaris does not itself require); pinning
# backward is refused here rather than at connect, where it would surface in
# the middle of an agent session.
PIN_TAG=""
REQUIRED_VERSION=""

# Written beside an installed binary so a later run can tell what it is.
MARKER_NAME=".moon-version"

say()  { printf 'install-moon: %s\n' "$*" >&2; }
warn() { printf 'install-moon: warning: %s\n' "$*" >&2; }
die()  { printf 'install-moon: error: %s\n' "$*" >&2; exit 1; }

have_cmd() { command -v "$1" >/dev/null 2>&1; }

# Strip a leading "v" and anything from the first "-" (so "0.8.5-dev" -> "0.8.5").
# A -dev build of the right number satisfies the floor: Lunaris' own version
# check treats 0.8.5-dev as 0.8.5 (version.rs -- operators routinely run one).
normalize_version() {
    printf '%s' "$1" | sed -e 's/^v//' -e 's/-.*$//'
}

# True when $1 >= $2, comparing dotted numeric components.
version_ge() {
    [ "$(printf '%s\n%s\n' "$2" "$1" | sort -t. -k1,1n -k2,2n -k3,3n | head -n1)" = "$2" ]
}

# The recorded version for a binary, or failure when unknown. NOT a probe of
# the binary itself -- Moon exposes no such surface (see header).
recorded_version() {
    _mk="$(dirname "$1")/${MARKER_NAME}"
    [ -r "$_mk" ] || return 1
    _v=$(normalize_version "$(head -n1 "$_mk" 2>/dev/null || true)")
    [ -n "$_v" ] || return 1
    printf '%s' "$_v"
}

# Prove a binary is actually runnable on this machine (not a truncated download
# or the wrong architecture). `--help` exits 0 and binds nothing.
runs_ok() {
    [ -x "$1" ] && "$1" --help >/dev/null 2>&1
}

resolve_install_dir() {
    if [ -n "${INSTALL_DIR:-}" ]; then
        printf '%s' "$INSTALL_DIR"
    elif [ "$(id -u)" = "0" ]; then
        printf '%s' /usr/local/bin
    else
        # ~/.local/bin is what Moon's own installer targets, and it is already
        # probed by scripts/setup-lunaris-agents.py's resolver. Landing here
        # means the agent setup finds this binary with no extra wiring.
        printf '%s' "${HOME}/.local/bin"
    fi
}

# ── Rung 1: reuse a Moon that is already on this machine ─────────────────────
#
# Two tiers, because we can only verify what we installed. A recorded version
# at or above the requirement wins outright. Otherwise the first runnable
# binary is reused with a warning -- see the header for why that is the right
# default rather than rebuilding or refusing.
find_existing_moon() {
    _unverified=""
    for _cand in \
        "${MOON_BIN:-}" \
        "$(command -v moon 2>/dev/null || true)" \
        "${HOME}/.local/bin/moon" \
        "${HOME}/.lunaris/bin/moon" \
        /usr/local/bin/moon
    do
        [ -n "$_cand" ] || continue
        runs_ok "$_cand" || continue
        if _rv=$(recorded_version "$_cand"); then
            if version_ge "$_rv" "$REQUIRED_VERSION"; then
                printf '%s %s' "$_cand" "$_rv"
                return 0
            fi
            # A recorded version BELOW the requirement is a definite miss: keep
            # looking rather than reusing a Moon we know to be too old.
            continue
        fi
        [ -n "$_unverified" ] || _unverified="$_cand"
    done
    if [ -n "$_unverified" ]; then
        printf '%s %s' "$_unverified" "unknown"
        return 0
    fi
    return 1
}

# ── Rung 2: a published release tarball for the pinned tag ───────────────────
# Dead as of 2026-08-25 (assets=0). Kept so installs speed up automatically if
# Moon's release workflow is repaired. Never fatal -- failure falls to rung 3.
try_prebuilt() {
    _dest="$1"
    have_cmd curl || return 1
    have_cmd tar  || return 1

    case "$(uname -s)" in
        Linux)  _os=unknown-linux-gnu ;;
        Darwin) _os=apple-darwin ;;
        *)      return 1 ;;
    esac
    case "$(uname -m)" in
        x86_64|amd64)  _arch=x86_64 ;;
        arm64|aarch64) _arch=aarch64 ;;
        *)             return 1 ;;
    esac

    _art="moon-${PIN_TAG}-${_arch}-${_os}.tar.gz"
    _url="${MOON_REPO}/releases/download/${PIN_TAG}/${_art}"

    # Probe before committing to a download, so the common (404) case is quiet.
    curl -fsSL -o /dev/null --connect-timeout 10 --max-time 30 -I "$_url" 2>/dev/null || return 1

    _work=$(mktemp -d) || return 1
    say "downloading ${_art}"
    if curl -fsSL --connect-timeout 30 --max-time 300 "$_url" -o "${_work}/${_art}" \
       && tar -xzf "${_work}/${_art}" -C "$_work"; then
        _found=$(find "$_work" -type f -name moon 2>/dev/null | head -n1)
        if [ -n "$_found" ] && install -m 0755 "$_found" "${_dest}/moon"; then
            rm -rf "$_work"
            return 0
        fi
    fi
    rm -rf "$_work"
    return 1
}

# ── Rung 3: build from the public source tag ─────────────────────────────────
#
# NOT `cargo install --git`. That is the obvious form and it fails for every
# anonymous user: cargo initialises submodules for a git source, and Moon's
# `.gitmodules` declares `.planning -> git@github.com:pilotspace/moon-docs.git`
# -- a PRIVATE repo behind an scp-style URL that cargo cannot even parse:
#
#   error: failed to update submodule `.planning`
#   invalid url `git@github.com:pilotspace/moon-docs.git`
#
# So we clone WITHOUT submodules and build from the checkout. `.planning` is
# documentation; nothing in Moon's build graph references it. Verified end to
# end against tag v0.8.5 on 2026-08-25.
build_from_source() {
    _root="$1"
    have_cmd git   || die "no prebuilt Moon for this platform and \`git\` was not found."
    have_cmd cargo || die "no prebuilt Moon is available for this platform and \
\`cargo\` was not found, so Moon cannot be built from source.
  Install Rust (1.94+), then re-run this script:
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"

    say "no prebuilt binary for ${PIN_TAG}; building Moon from source (a few minutes)"

    _src=$(mktemp -d) || die "could not create a temporary directory"

    # --depth 1: we build one tag, never walk history.
    # No --recurse-submodules, deliberately -- see the block comment above.
    if ! git clone --depth 1 --branch "$PIN_TAG" --quiet "$MOON_REPO" "${_src}/moon"; then
        rm -rf "$_src"
        die "could not clone ${MOON_REPO} at ${PIN_TAG}"
    fi

    # --locked: build the dependency set Moon tested against, not whatever
    # resolves today. --root: cargo appends /bin.
    if ! cargo install --path "${_src}/moon" --locked --root "$_root" --bin moon; then
        rm -rf "$_src"
        die "source build failed. Reproduce with:
    git clone --depth 1 --branch ${PIN_TAG} ${MOON_REPO} moon-src
    cargo install --path moon-src --locked --bin moon"
    fi
    rm -rf "$_src"
}

main() {
    # Resolve the pin before anything else, so a bad MOON_VERSION fails
    # immediately instead of after a multi-minute source build.
    _want=$(normalize_version "${MOON_VERSION:-$MIN_MOON_VERSION}")
    case "$_want" in
        ''|*[!0-9.]*) die "MOON_VERSION='${MOON_VERSION:-}' is not a version number (expected e.g. 0.8.7)" ;;
    esac
    version_ge "$_want" "$MIN_MOON_VERSION" || die \
        "MOON_VERSION=${_want} is below the ${MIN_MOON_VERSION} this Lunaris build requires; it would be refused at connect"
    REQUIRED_VERSION="$_want"
    PIN_TAG="v${_want}"

    if _hit=$(find_existing_moon); then
        _bin=${_hit% *}; _ver=${_hit##* }
        if [ "$_ver" = "unknown" ]; then
            say "reusing the Moon already at ${_bin} -- nothing to do"
            warn "its version could not be determined (Moon has no --version flag, and this script never starts a server). If it is older than ${REQUIRED_VERSION}, Lunaris will say so at connect."
        else
            say "Moon ${_ver} already installed at ${_bin} (>= ${REQUIRED_VERSION}) -- nothing to do"
        fi
        printf '%s\n' "$_bin"
        return 0
    fi

    _dir=$(resolve_install_dir)
    mkdir -p "$_dir" || die "cannot create install dir: $_dir"
    [ -w "$_dir" ] || die "install dir is not writable: $_dir (set INSTALL_DIR=)"

    say "installing Moon ${PIN_TAG} into ${_dir}"

    if ! try_prebuilt "$_dir"; then
        # cargo install --root DIR writes DIR/bin/moon. Give it the parent when
        # the target already ends in /bin, then normalise so callers always
        # find $_dir/moon.
        case "$_dir" in
            */bin) build_from_source "${_dir%/bin}" ;;
            *)     build_from_source "$_dir"
                   [ -x "${_dir}/bin/moon" ] && install -m 0755 "${_dir}/bin/moon" "${_dir}/moon"
                   ;;
        esac
    fi

    _out="${_dir}/moon"
    [ -x "$_out" ] || die "install completed but no executable at ${_out}"

    # Verify what we actually installed rather than trusting an exit code. We
    # cannot ask for a version, but we CAN prove the binary runs on this
    # machine -- which catches a truncated download or a wrong-arch tarball.
    runs_ok "$_out" || die "installed binary at ${_out} does not execute (\`moon --help\` failed)"

    # Record what we installed so the next run can short-circuit rung 1.
    printf '%s\n' "$REQUIRED_VERSION" > "${_dir}/${MARKER_NAME}" 2>/dev/null \
        || warn "could not write ${_dir}/${MARKER_NAME}; the next run will treat this Moon as unverified"

    say "installed Moon ${REQUIRED_VERSION} at ${_out}"
    case ":${PATH}:" in
        *":${_dir}:"*) ;;
        *) warn "${_dir} is not on your PATH; add it or pass --moon-bin ${_out}" ;;
    esac
    printf '%s\n' "$_out"
}

main "$@"
