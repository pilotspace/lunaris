#!/usr/bin/env python3
"""Lunaris owns its Moon install path — and points users at a live one.

Why this exists
---------------
Lunaris 0.7.0 refuses at connect any Moon below `MIN_MOON_VERSION`
(`crates/lunaris-storage-moon/src/version.rs`). Until 2026-08-25 every
install guide, and three Rust error strings, told the user to run:

    curl -fsSL https://raw.githubusercontent.com/pilotspace/moon/main/install.sh | sh

That command cannot succeed on any machine. Moon's installer downloads a
release tarball and, with no VERSION set, resolves to the *latest* tag —
and the three most recent Moon releases (v0.8.5, v0.8.6, v0.8.7) all
publish ZERO binary assets, while the ghcr image is private. So a user with
no Moon was handed a 404 by every surface Lunaris exposes, including the
one printed by `setup-lunaris-agents.py` when it fails to find a binary.
That is a closed loop: the tool that installs Lunaris told you to run a
command that could not work.

Lunaris must not be gated on another project's release pipeline.
`scripts/install-moon.sh` takes ownership: reuse an existing Moon, else a
published tarball, else build from the public source tag. Rung 3 always
works because `pilotspace/moon` is a public Apache-2.0 repo.

What this guard holds
---------------------
1. The installer exists, is executable, and is a real ladder — not a
   one-liner wrapper around the same dead download.
2. Its pinned version agrees with the engine's `MIN_MOON_VERSION`. An
   installer that fetches a Moon the engine then rejects at connect is
   worse than no installer: it fails later, inside an agent session,
   instead of at install time.
3. No user-facing surface still routes people to the dead one-liner.

On (3): this is deliberately a *routing* check, not a mention ban. Prose
may name Moon's own installer when explaining history or offering it as an
upstream alternative; what it may not do is present it as the command to
run. The discriminator is the `curl … | sh` pipeline, which is the form a
reader copies.
"""

from __future__ import annotations

import re
import stat
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / "scripts" / "install-moon.sh"
VERSION_RS = ROOT / "crates" / "lunaris-storage-moon" / "src" / "version.rs"

# Trees we do not own or do not ship to users.
EXCLUDED = ("vendor/", "target/", "node_modules/", ".add/", ".git/", ".planning/")

# The copy-paste form. A bare URL in prose is fine; a pipeline into a shell
# is an instruction.
DEAD_ONELINER = re.compile(
    r"curl[^\n|]*raw\.githubusercontent\.com/pilotspace/moon/[^\n|]*install\.sh[^\n]*\|\s*sh"
)

SEARCHED_SUFFIXES = {".md", ".rs", ".py", ".yml", ".yaml", ".sh", ".ts", ".mts", ".json"}


def tracked_files() -> list[Path]:
    out = subprocess.run(
        ["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout.splitlines()
    files = []
    for rel in out:
        if any(rel.startswith(x) or f"/{x}" in rel for x in EXCLUDED):
            continue
        p = ROOT / rel
        if p.suffix in SEARCHED_SUFFIXES and p.is_file():
            files.append(p)
    assert files, "git ls-files returned nothing — the scan would vacuously pass"
    return files


def test_installer_exists_and_is_executable() -> None:
    assert INSTALLER.is_file(), (
        f"{INSTALLER.relative_to(ROOT)} is missing. Lunaris is Moon-only, so the "
        "install path for Moon is part of Lunaris' own install story."
    )
    mode = INSTALLER.stat().st_mode
    assert mode & stat.S_IXUSR, (
        f"{INSTALLER.relative_to(ROOT)} is not executable. It is documented as a "
        "`curl … | sh` target, but a checkout user runs it directly."
    )


def test_installer_is_a_ladder_not_a_single_download() -> None:
    """A single strategy is exactly what broke. Require a real fallback.

    Without this, the installer could be 'fixed' by swapping one dead URL
    for another and the version-parity test below would still pass.
    """
    body = INSTALLER.read_text()
    for rung, needle in (
        ("reuse an existing Moon", "find_existing_moon"),
        ("published tarball fast path", "try_prebuilt"),
        ("source build fallback", "build_from_source"),
    ):
        assert needle in body, f"install-moon.sh has no {rung} rung (`{needle}`)"

    # `cargo install --git` is the obvious form and it CANNOT work: cargo
    # initialises submodules, and Moon's .gitmodules points .planning at a
    # private scp-style URL cargo cannot parse. Regressing to it would break
    # every anonymous install while still looking correct in review.
    #
    # Match on CODE, not on the file's own prose. The installer documents this
    # trap in a comment block, and a raw substring scan reports that comment as
    # the defect it warns about — the check would fail on a correct file and be
    # 'fixed' by deleting the explanation.
    code = "\n".join(
        line for line in body.splitlines() if not line.lstrip().startswith("#")
    )
    assert "cargo install --git" not in code, (
        "install-moon.sh uses `cargo install --git`, which fails for every "
        "anonymous user: cargo initialises Moon's `.planning` submodule, whose "
        "URL is `git@github.com:pilotspace/moon-docs.git` (private, scp-style) "
        "and cargo rejects it with 'relative URL without a base'. Clone with "
        "--depth 1 --branch <tag> and `cargo install --path` instead."
    )


def test_installer_pins_the_version_the_engine_requires() -> None:
    m = re.search(
        r"MIN_MOON_VERSION\s*:\s*MoonVersion\s*=\s*MoonVersion\s*\{\s*"
        r"major:\s*(\d+),\s*minor:\s*(\d+),\s*patch:\s*(\d+)",
        VERSION_RS.read_text(),
    )
    assert m, f"could not parse MIN_MOON_VERSION out of {VERSION_RS.relative_to(ROOT)}"
    engine = ".".join(m.groups())

    im = re.search(r'^MIN_MOON_VERSION="([^"]+)"', INSTALLER.read_text(), re.M)
    assert im, "install-moon.sh does not define MIN_MOON_VERSION"
    installer = im.group(1)

    assert installer == engine, (
        f"install-moon.sh pins Moon {installer} but the engine refuses anything "
        f"below {engine} at connect ({VERSION_RS.relative_to(ROOT)}). The install "
        "would appear to succeed and then fail inside an agent session."
    )


def test_no_surface_routes_users_to_the_dead_oneliner() -> None:
    offenders: list[str] = []
    for path in tracked_files():
        if path == INSTALLER or path.name == Path(__file__).name:
            continue  # both quote it to explain why it is wrong
        try:
            text = path.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        for i, line in enumerate(text.splitlines(), 1):
            if DEAD_ONELINER.search(line):
                offenders.append(f"{path.relative_to(ROOT)}:{i}")

    assert not offenders, (
        "these surfaces still tell users to run Moon's own installer, which "
        "resolves to the latest tag and 404s (v0.8.5/6/7 all ship 0 assets):\n  "
        + "\n  ".join(offenders)
        + "\n\nRoute them to `scripts/install-moon.sh` instead."
    )


if __name__ == "__main__":
    sys.exit(subprocess.call([sys.executable, "-m", "pytest", "-q", __file__]))
