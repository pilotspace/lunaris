#!/usr/bin/env python3
"""0.6.1 release-distribution guard — npm platform-package parity.

Why this exists
---------------
`npm install @pilotspace/lunaris` has been shipping WITHOUT its native
binary since 0.5.0. Two independent defects produced that outcome and
neither one is visible at install time, because napi platform packages
live in `optionalDependencies` — npm swallows a 404 on an optional
dependency and reports a successful install. The failure only surfaces
when user code calls `require('@pilotspace/lunaris')`.

Defect 1 (this test): `crates/lunaris-ts/package.json` declares
`"version": "0.6.0-rc.2"` but pins every optional dependency at
`"0.5.0"`. `scripts/bump-version.sh` rewrites `.version` and never
touches `.optionalDependencies`, so the pin froze at whatever release
last edited it by hand. CI publishes the platform packages at
`$VERSION`, so the main package asks for a version that was never
published — and the registry confirms it: `@pilotspace/lunaris` exists
at 0.3.0 and 0.5.0, while every `@pilotspace/lunaris-<platform>` exists
only at 0.3.0.

Defect 2 (fixed in .github/workflows/ts-prebuild.yml, not asserted
here): the publish loop wrapped each `npm publish` in
`|| echo "::warning::..."`, so six consecutive `npm error code E404`
responses in the v0.6.0-rc.1 tag run (29399540645) still produced a
green job.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_npm_version_parity.py
"""

from __future__ import annotations

import json
import re
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
TS_PACKAGE_JSON = REPO_ROOT / "crates" / "lunaris-ts" / "package.json"
TS_PACKAGE_LOCK = REPO_ROOT / "crates" / "lunaris-ts" / "package-lock.json"
TS_PREBUILD_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "ts-prebuild.yml"

# napi-rs target triple -> the npm/<dir> name `napi create-npm-dirs` emits,
# which is also the `@pilotspace/lunaris-<suffix>` package suffix.
TRIPLE_TO_PLATFORM = {
    "x86_64-unknown-linux-gnu": "linux-x64-gnu",
    "aarch64-unknown-linux-gnu": "linux-arm64-gnu",
    "x86_64-apple-darwin": "darwin-x64",
    "aarch64-apple-darwin": "darwin-arm64",
    "x86_64-pc-windows-msvc": "win32-x64-msvc",
}


def _load_package_json() -> dict:
    return json.loads(TS_PACKAGE_JSON.read_text(encoding="utf-8"))


class NpmOptionalDependencyParity(unittest.TestCase):
    """The main package must request the exact version CI publishes."""

    def test_every_optional_dependency_pins_the_package_version(self) -> None:
        pkg = _load_package_json()
        version = pkg["version"]
        optional = pkg.get("optionalDependencies", {})

        self.assertTrue(
            optional,
            "crates/lunaris-ts/package.json declares no optionalDependencies — "
            "the napi platform packages would never be resolved at all",
        )

        stale = {
            name: pinned
            for name, pinned in optional.items()
            if pinned != version
        }
        self.assertEqual(
            {},
            stale,
            f"optionalDependencies must pin the package's own version "
            f"({version!r}); CI publishes the platform packages at that "
            f"version and npm SILENTLY skips any that 404. Stale pins: "
            f"{stale}",
        )

    def test_optional_dependencies_cover_every_declared_napi_target(self) -> None:
        pkg = _load_package_json()
        optional = set(pkg.get("optionalDependencies", {}))
        triples = pkg["napi"]["targets"]

        unknown = [t for t in triples if t not in TRIPLE_TO_PLATFORM]
        self.assertEqual(
            [],
            unknown,
            "package.json declares a napi target this guard does not know how "
            "to map to an npm platform suffix; extend TRIPLE_TO_PLATFORM (and "
            "the ts-prebuild.yml publish loop) alongside the new target",
        )

        expected = {
            f"@pilotspace/lunaris-{TRIPLE_TO_PLATFORM[t]}" for t in triples
        }
        self.assertEqual(
            expected,
            optional,
            "every napi target must have a matching optionalDependency, or "
            "that platform installs binary-less",
        )


class WorkflowPublishRoster(unittest.TestCase):
    """The workflow must publish exactly the packages the main one requires."""

    def test_publish_loop_covers_every_optional_dependency(self) -> None:
        pkg = _load_package_json()
        optional = set(pkg.get("optionalDependencies", {}))

        workflow = TS_PREBUILD_WORKFLOW.read_text(encoding="utf-8")
        match = re.search(r"^\s*for d in (.+?); do\s*$", workflow, re.M)
        self.assertIsNotNone(
            match,
            "could not find the platform publish loop in ts-prebuild.yml; if "
            "the loop was restructured, update this guard so the roster stays "
            "pinned",
        )
        assert match is not None  # narrow for type checkers
        published = {
            f"@pilotspace/lunaris-{d}" for d in match.group(1).split()
        }

        self.assertEqual(
            optional,
            published,
            "ts-prebuild.yml publishes a different set of platform packages "
            "than @pilotspace/lunaris depends on — the difference installs as "
            "a silent 404",
        )




class NpmLockFileParity(unittest.TestCase):
    """`npm ci` refuses to run when the lock disagrees with package.json.

    This is not hypothetical. The v0.7.0 release published the five
    `@pilotspace/lunaris-*` platform packages on 2026-08-19 after a long
    E404/EOTP token fight; `package.json` had already been bumped to require
    `0.7.0`, but `package-lock.json` still carried each platform package with
    NO version at all -- the shape npm records for an optional dependency it
    could not resolve at lock time. From then on every `npm ci` in CI failed
    EUSAGE ("lock file's @pilotspace/lunaris-darwin-arm64@ does not satisfy
    ... @0.7.0"), which took down BOTH conformance-bindings jobs nightly.

    The sibling tests in this file all read package.json only, so a lock that
    disagrees with it was invisible to them. This closes that gap: the lock is
    a published artifact of the release process and must be re-generated
    whenever the version moves (`npm install --package-lock-only`).
    """

    def _lock_versions(self) -> dict:
        lock = json.loads(TS_PACKAGE_LOCK.read_text(encoding="utf-8"))
        out = {}
        for path, meta in lock.get("packages", {}).items():
            name = path.split("node_modules/")[-1]
            if name.startswith("@pilotspace/"):
                out[name] = meta.get("version")
        return out

    def test_lock_file_pins_the_same_versions_as_package_json(self) -> None:
        optional = _load_package_json().get("optionalDependencies", {})
        locked = self._lock_versions()

        mismatched = {
            name: (locked.get(name), want)
            for name, want in optional.items()
            if locked.get(name) != want
        }
        self.assertEqual(
            mismatched,
            {},
            "crates/lunaris-ts/package-lock.json disagrees with package.json "
            f"(name: locked -> required): {mismatched}. `npm ci` will fail "
            "EUSAGE and every workflow that installs lunaris-ts dies at that "
            "step. A `None` on the locked side means the package was "
            "unpublished when the lock was written. Fix by running "
            "`npm install --package-lock-only` in crates/lunaris-ts and "
            "committing the result.",
        )

    def test_lock_file_records_every_optional_dependency(self) -> None:
        optional = set(_load_package_json().get("optionalDependencies", {}))
        locked = set(self._lock_versions())
        missing = sorted(optional - locked)
        self.assertEqual(
            missing,
            [],
            f"package-lock.json has no entry at all for {missing}. Regenerate "
            "it with `npm install --package-lock-only` in crates/lunaris-ts.",
        )


class EntryPointsArePublished(unittest.TestCase):
    """Every entry point npm resolves must be in `files` and on disk.

    `files` is an allowlist: anything absent from it is silently omitted from
    the published tarball. So a package can declare `"require": "./lunaris.cjs"`,
    pass every test in the repo, publish successfully, and then throw
    MODULE_NOT_FOUND the first time a consumer calls `require()` — because the
    file the export map points at was never shipped. Nothing in this repo packs
    a tarball and inspects it, so `files` is the only thing standing between the
    export map and that outcome.

    Live risk rather than a hypothetical: the v0.6 SDK rewrite moved the CJS
    entry from `index.js` to a new `lunaris.cjs` and added `lunaris.d.ts`
    alongside it. Two new filenames, both of which had to be added to `files`
    by hand, in a package whose optional dependencies had already frozen once
    (see this file's module docstring) for exactly that kind of manual-edit
    miss.
    """

    def _referenced_paths(self) -> set[str]:
        pkg = _load_package_json()
        refs = set()
        for key in ("main", "module", "types", "browser", "bin"):
            v = pkg.get(key)
            if isinstance(v, str):
                refs.add(v)
            elif isinstance(v, dict):
                refs.update(x for x in v.values() if isinstance(x, str))

        def walk(node) -> None:
            if isinstance(node, str):
                refs.add(node)
            elif isinstance(node, dict):
                for v in node.values():
                    walk(v)

        walk(pkg.get("exports", {}))
        return {r[2:] if r.startswith("./") else r for r in refs}

    def test_every_entry_point_is_listed_in_files(self) -> None:
        refs = self._referenced_paths()
        # Vacuity floor: an export map that resolves to nothing would otherwise
        # make both assertions below pass over an empty set.
        self.assertGreaterEqual(
            len(refs),
            3,
            f"expected at least 3 entry points (main/module/types), found "
            f"{sorted(refs)}. If package.json's shape changed, update this "
            f"guard rather than letting it scan nothing.",
        )

        listed = set(_load_package_json().get("files", []))
        missing = sorted(r for r in refs if r not in listed)
        self.assertEqual(
            [],
            missing,
            f"these entry points are referenced by package.json but absent from "
            f'its "files" allowlist: {missing}. npm omits them from the '
            f"published tarball, so the package installs cleanly and then fails "
            f"with MODULE_NOT_FOUND on first use.",
        )

    def test_every_entry_point_exists_on_disk(self) -> None:
        pkg_dir = TS_PACKAGE_JSON.parent
        missing = sorted(r for r in self._referenced_paths() if not (pkg_dir / r).is_file())
        self.assertEqual(
            [],
            missing,
            f"package.json points at files that do not exist: {missing}. Listing "
            f"a path in `files` does not create it; npm publishes the package "
            f"without them and the export map resolves to nothing.",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
