#!/usr/bin/env python3
"""SessionStart digest leg — inject curated recent decisions, never block.

ADD task `session-start-digest` (2026-07-14). `run_session_digest` sends a
`session_digest` request to contextd and emits the rendered digest as
SessionStart additionalContext. Design-for-failure: a cold/down daemon, a
timeout, or an empty digest must still exit 0 with no injection — a digest is a
nicety, never a gate on the session starting.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_session_digest.py
"""

from __future__ import annotations

import importlib.util
import io
import unittest
from contextlib import redirect_stdout
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ADAPTER = ROOT / "scripts" / "lunaris-codex-hook-adapter.py"


def load_adapter():
    spec = importlib.util.spec_from_file_location("lunaris_codex_hook_adapter", ADAPTER)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


ADP = load_adapter()

SESSION_START = {"hook_event_name": "SessionStart", "cwd": str(ROOT), "session_id": "s1"}


class SessionDigestLeg(unittest.TestCase):
    def setUp(self):
        self._orig = ADP.contextd_request
        self.calls: list[dict] = []

    def tearDown(self):
        ADP.contextd_request = self._orig

    def test_sends_session_digest_request(self):
        def _spy(request, timeout_ms, autostart=True):
            self.calls.append(request)
            return {"ok": True, "rendered_context": "", "memories": []}

        ADP.contextd_request = _spy
        with redirect_stdout(io.StringIO()):
            rc = ADP.run_session_digest(SESSION_START, "claude")
        self.assertEqual(rc, 0)
        self.assertEqual(len(self.calls), 1)
        self.assertEqual(self.calls[0]["type"], "session_digest")
        self.assertIn("cwd", self.calls[0])

    def test_empty_digest_exits_zero_no_injection(self):
        def _spy(request, timeout_ms, autostart=True):
            self.calls.append(request)
            return {"ok": True, "memories": [], "rendered_context": ""}

        ADP.contextd_request = _spy
        buf = io.StringIO()
        with redirect_stdout(buf):
            rc = ADP.run_session_digest(SESSION_START, "claude")
        self.assertEqual(rc, 0)
        # nothing injected for an empty digest
        self.assertNotIn("additionalContext", buf.getvalue())

    def test_daemon_down_still_exits_zero(self):
        def _boom(request, timeout_ms, autostart=True):
            raise ConnectionError("contextd down")

        ADP.contextd_request = _boom
        with redirect_stdout(io.StringIO()):
            rc = ADP.run_session_digest(SESSION_START, "claude")
        self.assertEqual(rc, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
