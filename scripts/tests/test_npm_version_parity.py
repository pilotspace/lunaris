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


if __name__ == "__main__":
    unittest.main(verbosity=2)
