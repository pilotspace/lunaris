#!/usr/bin/env python3
"""Capture signal-gate — only high-value hook events become durable memories.

ADD task `capture-signal-gate` (2026-07-14). The adapter's `run_capture`
unconditionally forwarded every pre/posttooluse event's WHOLE envelope
(cwd, duration_ms, permission_mode, prompt_id + any tool body) to contextd,
which stores it raw. Contentless / low-value events became durable noise.

`capture_has_signal(event)` is a pure, fail-OPEN predicate: keep an event iff
it carries a touched path or a non-empty content field; drop metadata-only
envelopes and LOW_VALUE_TOOLS. `run_capture` consults it before any contextd
send (both the fast-path and the subprocess path) and skips (returns 0) on a
drop. `LUNARIS_CONTEXT_CAPTURE_GATE=off` restores capture-everything.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_capture_signal_gate.py
"""

from __future__ import annotations

import importlib.util
import os
import unittest
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


def _post(**extra):
    """A posttooluse-shaped envelope with the given extra keys."""
    base = {
        "hook_event_name": "PostToolUse",
        "cwd": str(ROOT),
        "duration_ms": 12,
        "permission_mode": "default",
        "prompt_id": "abc123",
    }
    base.update(extra)
    return base


class CaptureHasSignal(unittest.TestCase):
    def test_metadata_only_event_is_dropped(self):
        self.assertFalse(ADP.capture_has_signal(_post(tool_name="Edit")))

    def test_edit_event_with_path_is_kept(self):
        ev = _post(
            tool_name="Edit",
            tool_input={"file_path": "/x/y.rs", "new_string": "fn main() {}"},
        )
        self.assertTrue(ADP.capture_has_signal(ev))

    def test_bash_output_is_kept(self):
        ev = _post(tool_name="Bash", tool_response="compiled 3 crates")
        self.assertTrue(ADP.capture_has_signal(ev))

    def test_low_value_tool_is_dropped(self):
        ev = _post(tool_name="pwd", tool_response="/home/x")
        self.assertFalse(ADP.capture_has_signal(ev))

    def test_malformed_event_fails_open(self):
        # non-dict -> fail open (True), and must not raise
        self.assertTrue(ADP.capture_has_signal("not-a-dict"))
        self.assertTrue(ADP.capture_has_signal(None))


class RunCaptureGating(unittest.TestCase):
    def setUp(self):
        self._orig_req = ADP.contextd_request
        self.calls: list[dict] = []

        def _spy(request, timeout_ms, autostart=True):
            self.calls.append(request)
            return {}

        ADP.contextd_request = _spy
        # Ensure the fast-path (which calls contextd_request) is active.
        os.environ["LUNARIS_CONTEXT_CAPTURE_FAST"] = "1"
        os.environ.pop("LUNARIS_CONTEXT_CAPTURE_GATE", None)

    def tearDown(self):
        ADP.contextd_request = self._orig_req
        os.environ.pop("LUNARIS_CONTEXT_CAPTURE_GATE", None)
        os.environ.pop("LUNARIS_CONTEXT_CAPTURE_FAST", None)

    def test_run_capture_skips_on_no_signal(self):
        rc = ADP.run_capture(_post(tool_name="Edit"))
        self.assertEqual(rc, 0)
        self.assertEqual(self.calls, [])

    def test_run_capture_sends_on_signal(self):
        ev = _post(
            tool_name="Edit",
            tool_input={"file_path": "/x/y.rs", "new_string": "fn main() {}"},
        )
        rc = ADP.run_capture(ev)
        self.assertEqual(rc, 0)
        self.assertGreaterEqual(len(self.calls), 1)

    def test_kill_switch_bypasses_gate(self):
        os.environ["LUNARIS_CONTEXT_CAPTURE_GATE"] = "off"
        rc = ADP.run_capture(_post(tool_name="Edit"))
        self.assertEqual(rc, 0)
        self.assertGreaterEqual(len(self.calls), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
