#!/usr/bin/env python3
"""ADD task `turnkey-moon-curl-install` (contract FROZEN @ v1, 2026-07-14).

Red-first suite: the turnkey script requires a curl-installed Moon and is
Moon-only.

  - Moon binary resolution: explicit --moon-bin > `moon` on PATH >
    ~/.local/bin/moon (curl install target) > vendored build artifact.
  - Missing binaries fail with the exact Moon curl install one-liner.
  - --storage-backend sqlite is rejected (Lunaris turnkey is Moon-only).
  - --build-moon is deprecated (kept as a dev fallback) and says so.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_turnkey_moon_only.py
"""

from __future__ import annotations

import argparse
import importlib.util
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SETUP = ROOT / "scripts" / "setup-lunaris-agents.py"
CURL_MARKER = "curl -fsSL"


def load_setup_module():
    spec = importlib.util.spec_from_file_location("setup_lunaris_agents", SETUP)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def run_setup(*argv: str, timeout: int = 60) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SETUP), *argv],
        capture_output=True,
        text=True,
        timeout=timeout,
        cwd=str(ROOT),
    )


class MoonOnlyBackend(unittest.TestCase):
    """§2 scenario 1 — sqlite backend rejected as Moon-only."""

    def test_sqlite_backend_rejected_moon_only(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            settings = Path(tmp) / "settings.json"
            proc = run_setup(
                "--agent",
                "claude",
                "--runner",
                "local",
                "--hooks",
                "off",
                "--storage-backend",
                "sqlite",
                "--claude-settings",
                str(settings),
                "--dry-run",
            )
            self.assertNotEqual(proc.returncode, 0, "sqlite backend must be rejected")
            combined = proc.stdout + proc.stderr
            self.assertIn("Moon-only", combined, f"must name the Moon-only policy:\n{combined}")
            self.assertIn(CURL_MARKER, combined, f"must suggest the curl install:\n{combined}")
            self.assertFalse(settings.exists(), "rejection must not write settings")


class MoonBinaryResolution(unittest.TestCase):
    """§2 scenarios 2–4 — binary resolution order + curl-hinted failures."""

    def test_explicit_missing_moon_bin_errors_with_curl_hint(self) -> None:
        missing = "/nonexistent/lunaris-test/moon"
        proc = run_setup(
            "--agent",
            "claude",
            "--runner",
            "local",
            "--hooks",
            "off",
            "--moon-bin",
            missing,
            "--dry-run",
        )
        self.assertNotEqual(proc.returncode, 0, "explicit missing --moon-bin must fail")
        combined = proc.stdout + proc.stderr
        self.assertIn(missing, combined, f"must name the missing path:\n{combined}")
        self.assertIn(CURL_MARKER, combined, f"must suggest the curl install:\n{combined}")

    def test_path_installed_moon_resolves(self) -> None:
        module = load_setup_module()
        with tempfile.TemporaryDirectory() as tmp:
            fake = Path(tmp) / "moon"
            fake.write_text("#!/bin/sh\nexit 0\n")
            fake.chmod(fake.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
            args = argparse.Namespace(moon_bin=str(module.MOON_BIN))
            with mock.patch.dict(os.environ, {"PATH": tmp}):
                resolved = module.resolve_moon_bin(args)
            self.assertEqual(
                Path(resolved), fake, "moon on PATH must win over the vendored artifact"
            )

    def test_local_bin_moon_resolves_when_not_on_path(self) -> None:
        module = load_setup_module()
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            local_moon = home / ".local" / "bin" / "moon"
            local_moon.parent.mkdir(parents=True)
            local_moon.write_text("#!/bin/sh\nexit 0\n")
            local_moon.chmod(local_moon.stat().st_mode | stat.S_IXUSR)
            args = argparse.Namespace(moon_bin=str(module.MOON_BIN))
            with (
                mock.patch.dict(os.environ, {"PATH": str(home / "empty")}),
                mock.patch.object(module.Path, "home", return_value=home),
            ):
                resolved = module.resolve_moon_bin(args)
            self.assertEqual(
                Path(resolved),
                local_moon,
                "~/.local/bin/moon (curl install target) must resolve even off PATH",
            )

    def test_vendored_binary_is_last_fallback(self) -> None:
        module = load_setup_module()
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            vendored = home / "vendored-moon"
            vendored.write_text("#!/bin/sh\nexit 0\n")
            vendored.chmod(vendored.stat().st_mode | stat.S_IXUSR)
            args = argparse.Namespace(moon_bin=str(vendored))
            with (
                mock.patch.dict(os.environ, {"PATH": str(home / "empty")}),
                mock.patch.object(module.Path, "home", return_value=home),
                mock.patch.object(module, "MOON_BIN", vendored),
            ):
                resolved = module.resolve_moon_bin(args)
            self.assertEqual(Path(resolved), vendored, "vendored artifact is the last fallback")

    def test_no_binary_anywhere_returns_none(self) -> None:
        module = load_setup_module()
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            ghost = home / "ghost-moon"
            args = argparse.Namespace(moon_bin=str(ghost))
            with (
                mock.patch.dict(os.environ, {"PATH": str(home / "empty")}),
                mock.patch.object(module.Path, "home", return_value=home),
                mock.patch.object(module, "MOON_BIN", ghost),
            ):
                resolved = module.resolve_moon_bin(args)
            self.assertIsNone(resolved, "no binary anywhere must resolve to None")


class BuildMoonDeprecated(unittest.TestCase):
    """§2 scenario 5 — --build-moon kept but deprecated in favor of curl."""

    def test_build_moon_prints_deprecation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            settings = Path(tmp) / "settings.json"
            proc = run_setup(
                "--agent",
                "claude",
                "--runner",
                "local",
                "--hooks",
                "off",
                "--build-moon",
                "--claude-settings",
                str(settings),
                "--dry-run",
            )
            self.assertEqual(
                proc.returncode, 0, f"--build-moon dry-run must still work:\n{proc.stderr}"
            )
            combined = proc.stdout + proc.stderr
            self.assertIn(
                "deprecat", combined.lower(), f"must print a deprecation notice:\n{combined}"
            )
            self.assertIn(CURL_MARKER, combined, f"deprecation must point at curl:\n{combined}")


class DocsCurlFirst(unittest.TestCase):
    """§1 Must (docs) — turnkey leads with the curl install, not --build-moon."""

    def test_turnkey_docs_lead_with_curl_install(self) -> None:
        text = (ROOT / "docs" / "integration" / "claude-code.md").read_text()
        self.assertIn(
            CURL_MARKER,
            text,
            "turnkey docs must show the Moon curl install",
        )
        self.assertIn(
            "setup-lunaris-agents.py --agent claude --runner local\n",
            text,
            "command 1 must not require --build-moon",
        )

    def test_readme_drops_sqlite_opt_out(self) -> None:
        text = (ROOT / "README.md").read_text()
        self.assertNotIn(
            "--storage-backend sqlite` to opt out",
            text,
            "README must not advertise a sqlite opt-out (Moon-only)",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
