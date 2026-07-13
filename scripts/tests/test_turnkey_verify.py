#!/usr/bin/env python3
"""ADD task `claude-code-turnkey` (contract FROZEN @ v1, 2026-07-14).

Red-first suite for the two-command turnkey proof:
  1. scripts/setup-lunaris-agents.py --agent claude --runner local ...
  2. scripts/setup-lunaris-agents.py --agent claude --verify

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_turnkey_verify.py

The live end-to-end test is gated on LUNARIS_HOOK_TEST_MOON_URL (moon-it
pattern) and needs target/release/{lunaris-hook,lunaris-contextd} prebuilt.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SETUP = ROOT / "scripts" / "setup-lunaris-agents.py"
DOCS = ROOT / "docs" / "integration" / "claude-code.md"
HOOK_BIN = ROOT / "target" / "release" / "lunaris-hook"
CONTEXTD_BIN = ROOT / "target" / "release" / "lunaris-contextd"


def run_setup(*argv: str, timeout: int = 120) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SETUP), *argv],
        capture_output=True,
        text=True,
        timeout=timeout,
        cwd=str(ROOT),
    )


class VerifyFailFast(unittest.TestCase):
    """§2 scenarios 2+3 — verify must fail fast and actionably, never hang."""

    def test_verify_missing_settings_fails_actionably(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            missing = Path(tmp) / "nope" / "settings.json"
            proc = run_setup(
                "--agent",
                "claude",
                "--verify",
                "--claude-settings",
                str(missing),
                timeout=30,
            )
            self.assertNotEqual(proc.returncode, 0, "missing settings must fail")
            combined = proc.stdout + proc.stderr
            self.assertIn(
                str(missing),
                combined,
                f"failure must name the settings path; got:\n{combined}",
            )
            self.assertIn(
                "setup-lunaris-agents.py --agent claude",
                combined,
                f"failure must point at the setup command; got:\n{combined}",
            )
            self.assertFalse(missing.exists(), "verify must not create settings")

    def test_verify_unreachable_storage_fails_fast(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            settings = Path(tmp) / "settings.json"
            wrote = run_setup(
                "--agent",
                "claude",
                "--runner",
                "local",
                "--no-build-hooks",
                "--hooks",
                "on" if HOOK_BIN.exists() else "off",
                "--claude-settings",
                str(settings),
                "--storage-url",
                "moon://127.0.0.1:1",
                timeout=60,
            )
            if wrote.returncode != 0:
                self.skipTest(f"setup prerequisites missing: {wrote.stderr.strip()}")
            proc = run_setup(
                "--agent",
                "claude",
                "--verify",
                "--no-moon-autostart",
                "--claude-settings",
                str(settings),
                "--storage-url",
                "moon://127.0.0.1:1",
                timeout=60,
            )
            self.assertNotEqual(proc.returncode, 0, "unreachable storage must fail")
            combined = proc.stdout + proc.stderr
            self.assertIn(
                "VERIFY FAIL: storage",
                combined,
                f"must name the storage stage in the contracted format:\n{combined}",
            )
            self.assertIn("moon", combined.lower(), f"must print a Moon launch recipe:\n{combined}")


class DocsLead(unittest.TestCase):
    """§2 scenario 4 — the docs page leads with the two commands."""

    def test_docs_lead_with_two_commands(self) -> None:
        text = DOCS.read_text()
        self.assertIn(
            "setup-lunaris-agents.py --agent claude --runner local --build-moon",
            text,
            "command 1 (setup) missing from docs/integration/claude-code.md",
        )
        self.assertIn(
            "setup-lunaris-agents.py --agent claude --verify",
            text,
            "command 2 (--verify) missing from docs/integration/claude-code.md",
        )


class TwoCommandTurnkey(unittest.TestCase):
    """§2 scenario 1 — the full proof against live Moon (gated)."""

    def test_two_command_turnkey_proves_capture_and_inject(self) -> None:
        moon_url = os.environ.get("LUNARIS_HOOK_TEST_MOON_URL")
        if not moon_url:
            self.skipTest("LUNARIS_HOOK_TEST_MOON_URL not set (live-Moon gate)")
        if not HOOK_BIN.exists() or not CONTEXTD_BIN.exists():
            self.skipTest("release hook binaries not built (cargo build --release -p lunaris-hook)")

        scope = f"lunaris-verify-{uuid.uuid4().hex[:12]}"
        with tempfile.TemporaryDirectory() as tmp:
            settings = Path(tmp) / "settings.json"
            # Command 1 — install (isolated settings path, prebuilt binaries).
            setup = run_setup(
                "--agent",
                "claude",
                "--runner",
                "local",
                "--no-build-hooks",
                "--claude-settings",
                str(settings),
                "--storage-url",
                moon_url,
                "--scope",
                scope,
                timeout=120,
            )
            self.assertEqual(setup.returncode, 0, f"setup failed:\n{setup.stdout}\n{setup.stderr}")
            written = json.loads(settings.read_text())
            self.assertIn("lunaris", written.get("mcpServers", {}), "setup must register MCP")
            self.assertIn("UserPromptSubmit", written.get("hooks", {}), "setup must install hooks")

            # Command 2 — the proof.
            verify = run_setup(
                "--agent",
                "claude",
                "--verify",
                "--claude-settings",
                str(settings),
                "--storage-url",
                moon_url,
                "--scope",
                scope,
                timeout=180,
            )
            combined = verify.stdout + verify.stderr
            self.assertIn(
                "VERIFY PASS: capture",
                verify.stdout,
                f"capture stage must pass; got:\n{combined}",
            )
            self.assertIn(
                "VERIFY PASS: cross-session inject",
                verify.stdout,
                f"cross-session inject stage must pass; got:\n{combined}",
            )
            self.assertEqual(verify.returncode, 0, f"verify must exit 0; got:\n{combined}")


if __name__ == "__main__":
    unittest.main(verbosity=2)
