#!/usr/bin/env python3
"""Two things about the distributions this repo ships.

1. A documented `pip install X` must name something CI actually publishes.
2. Everything CI publishes must carry the workspace version.

The failure this closes is quiet and it has already happened twice in this
repo. `lunaris-cli`'s manifest advertised "npx / uvx / GitHub Release
binaries" while the name was absent from crates.io, npm, PyPI and every
release asset; that comment was corrected by hand on 2026-08-21 after somebody
went and checked all four registries. `integrations/README.md` opened with
`pip install "lunaris-integrations[langgraph]"` while nothing in
`.github/workflows/` built, let alone uploaded, that distribution.

A reader cannot tell the difference between a command that works and one that
404s until they run it, and the first thing a stranger does with a public repo
is run the first command in the README.

The rule is keyed on the DECISION, not on any phrasing: if a first-party
distribution's name appears in an install command anywhere in the docs, some
workflow must publish that distribution. It therefore covers a caveat somebody
writes next in words nobody predicted, and it covers a distribution added
after this file was written.

The second is the same failure in the other direction. `bump-version.sh`
walks a hand-written list of manifests, and a manifest missing from that list
is not a build error — it just ships the previous version's number.
`integrations/pyproject.toml` sat at `0.7.0` through the entire 0.7.1 release
because nothing put it on the list, and `crates/lunaris-ts/package-lock.json`
being off the list broke two releases before somebody added it. Deriving the
set from "what does a workflow publish?" means the check covers the manifest
added next without anybody remembering to extend it.

Run directly (`python3 scripts/tests/test_published_distributions.py`) or
under pytest; `ci.yml` discovers every `scripts/tests/test_*.py`.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# Directories that hold vendored, generated or third-party manifests.
SKIP_DIRS = {
    "target", "node_modules", ".git", "vendor", ".add", ".planning",
    "dist", "build", ".venv", "venv", "__pycache__",
}

# The verbs that put bytes in front of a stranger.
PUBLISH_VERBS = ("twine upload", "maturin publish", "npm publish", "cargo publish")


def _walk(root: Path, filename: str) -> list[Path]:
    out: list[Path] = []
    for p in root.rglob(filename):
        if any(part in SKIP_DIRS for part in p.relative_to(root).parts):
            continue
        out.append(p)
    return out


def first_party_distributions() -> dict[str, Path]:
    """Distribution name -> the manifest that declares it."""
    dists: dict[str, Path] = {}

    for p in _walk(ROOT, "pyproject.toml"):
        text = p.read_text(encoding="utf-8")
        m = re.search(r'^\s*name\s*=\s*["\']([^"\']+)["\']', text, re.M)
        if m:
            dists.setdefault(m.group(1), p)

    for p in _walk(ROOT, "package.json"):
        try:
            data = json.loads(p.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError):
            continue
        name = data.get("name")
        if isinstance(name, str) and name:
            dists.setdefault(name, p)

    return dists


def published_distributions(dists: dict[str, Path]) -> set[str]:
    """The subset some workflow both references and publishes."""
    workflows = sorted((ROOT / ".github" / "workflows").glob("*.y*ml"))
    assert workflows, "no workflows found — this scan would pass vacuously"

    published: set[str] = set()
    for wf in workflows:
        text = wf.read_text(encoding="utf-8")
        if not any(v in text for v in PUBLISH_VERBS):
            continue
        for name, manifest in dists.items():
            manifest_dir = manifest.parent.relative_to(ROOT).as_posix()
            # Either the workflow names the distribution outright, or it names
            # the directory whose manifest declares it.
            if name in text or (manifest_dir and manifest_dir in text):
                published.add(name)
    return published


# `pip install foo`, `pip install "foo[extra]"`, `uv add foo`, `npm i foo`,
# `pnpm add foo`, `cargo install foo`. A leading `-` (a flag such as `-e` or
# `--git`) or a path separator means the command is not naming a registry
# distribution, and the `[^\s"'\[]` class stops before an extras group.
_INSTALL = re.compile(
    r"""(?:pip[3]?\s+install|uv\s+add|uv\s+pip\s+install
        |npm\s+(?:i|install)|pnpm\s+add|yarn\s+add|cargo\s+install)
        \s+
        (?!-)                      # not a flag
        ["']?
        (?P<name>[A-Za-z0-9@._/-]+)
    """,
    re.X,
)


def install_claims() -> list[tuple[Path, int, str]]:
    """Every (file, line number, distribution) an install command names."""
    claims: list[tuple[Path, int, str]] = []
    for md in _walk(ROOT, "*.md"):
        try:
            text = md.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        for i, line in enumerate(text.splitlines(), start=1):
            for m in _INSTALL.finditer(line):
                name = m.group("name")
                if "/" in name and not name.startswith("@"):
                    continue  # a path install, not a registry name
                claims.append((md, i, name))
    return claims


def test_every_documented_install_names_something_ci_publishes() -> None:
    dists = first_party_distributions()
    assert len(dists) >= 3, (
        f"found only {len(dists)} first-party distributions — the manifest scan "
        "is broken, and a broken scan reports a clean repo"
    )

    published = published_distributions(dists)
    assert published, "no distribution is published by any workflow — scan broken"

    claims = install_claims()
    assert claims, "no install command found in any .md — the doc scan is broken"

    offenders = [
        f"{p.relative_to(ROOT).as_posix()}:{ln}: `{name}`"
        for p, ln, name in claims
        if name in dists and name not in published
    ]

    assert not offenders, (
        "these docs tell a reader to install a distribution no workflow "
        f"publishes:\n  " + "\n  ".join(sorted(offenders)) + "\n\n"
        "Either add a workflow that publishes it, or stop naming it in an "
        "install command. A command that 404s is worse than no command: the "
        "reader concludes the project is broken, not that the doc is stale."
    )


def workspace_version() -> str:
    """`[workspace.package] version` from the root Cargo.toml."""
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    block = text.split("[workspace.package]", 1)
    assert len(block) == 2, "root Cargo.toml has no [workspace.package] section"
    m = re.search(r'^\s*version\s*=\s*"([^"]+)"', block[1], re.M)
    assert m, "no version in [workspace.package]"
    return m.group(1)


def manifest_version(manifest: Path) -> str | None:
    text = manifest.read_text(encoding="utf-8")
    if manifest.name == "package.json":
        v = json.loads(text).get("version")
        return v if isinstance(v, str) else None
    m = re.search(r'^\s*version\s*=\s*["\']([^"\']+)["\']', text, re.M)
    return m.group(1) if m else None


def test_every_published_manifest_is_at_the_workspace_version() -> None:
    dists = first_party_distributions()
    published = published_distributions(dists)
    assert published, "no distribution is published by any workflow — scan broken"

    want = workspace_version()
    offenders = []
    for name in sorted(published):
        manifest = dists[name]
        got = manifest_version(manifest)
        if got is None:
            # A Cargo.toml member inheriting `version.workspace = true` is
            # correct by construction and declares no literal.
            continue
        if got != want:
            offenders.append(
                f"{manifest.relative_to(ROOT).as_posix()}: {name} is {got}, "
                f"workspace is {want}"
            )

    assert not offenders, (
        "these published manifests disagree with the workspace version:\n  "
        + "\n  ".join(offenders)
        + "\n\nAdd the file to scripts/bump-version.sh. A manifest off that list "
        "is not a build error — it silently publishes the previous release's "
        "number, which is how the same artefact ends up on two registries under "
        "two different versions."
    )


if __name__ == "__main__":
    failed = False
    for fn in (
        test_every_documented_install_names_something_ci_publishes,
        test_every_published_manifest_is_at_the_workspace_version,
    ):
        try:
            fn()
        except AssertionError as e:
            print(f"FAIL {fn.__name__}: {e}", file=sys.stderr)
            failed = True
    if failed:
        raise SystemExit(1)
    print("ok: install claims are publishable, and every published manifest "
          "is at the workspace version")
