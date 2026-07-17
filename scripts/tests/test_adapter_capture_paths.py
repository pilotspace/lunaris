#!/usr/bin/env python3
"""ADD task `git-anchoring` (contract FROZEN @ v1, engram-soul-loop task 5).

`run_capture`'s fast-path payloads (`capture_tool_call` / `capture_tool_result`)
must forward `extract_paths(event) or None` as `"paths"` so the server can
stamp `meta.files` on the corresponding capture
(`.add/tasks/git-anchoring/TASK.md` §3 CONTRACT). Before this change neither
payload carried a `paths` key at all — `extract_paths` already exists and
feeds `recall_after_tool`'s `paths` field (`run_post_tool_injection`), but
`run_capture` never wired it into either capture payload.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_adapter_capture_paths.py
"""

from __future__ import annotations

import importlib.util
import os
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
ADAPTER = ROOT / "scripts" / "lunaris-codex-hook-adapter.py"


def load_adapter():
    spec = importlib.util.spec_from_file_location("lunaris_codex_hook_adapter", ADAPTER)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class RunCaptureForwardsPaths(unittest.TestCase):
    def setUp(self) -> None:
        # Force the fast-path (which builds the capture_tool_call /
        # capture_tool_result payloads under test) and the signal-gate's
        # default-on behavior, regardless of what a prior test in the same
        # process may have left in os.environ.
        os.environ["LUNARIS_CONTEXT_CAPTURE_FAST"] = "1"
        os.environ.pop("LUNARIS_CONTEXT_CAPTURE_GATE", None)

    def tearDown(self) -> None:
        os.environ.pop("LUNARIS_CONTEXT_CAPTURE_FAST", None)
        os.environ.pop("LUNARIS_CONTEXT_CAPTURE_GATE", None)

    def _capture(self, module, event: dict) -> tuple[int, list[dict]]:
        captured: list[dict] = []

        def fake_request(payload, **kwargs):
            captured.append(dict(payload))
            return {}

        with mock.patch.object(module, "contextd_request", side_effect=fake_request):
            rc = module.run_capture(event)
        return rc, captured

    def test_pretooluse_forwards_extracted_paths(self) -> None:
        module = load_adapter()
        event = {
            "hook_event_name": "PreToolUse",
            "cwd": str(ROOT),
            "session_id": "s-1",
            "tool_name": "Edit",
            "tool_input": {"file_path": "/x/y.rs", "new_string": "fn main() {}"},
        }
        rc, captured = self._capture(module, event)

        self.assertEqual(rc, 0)
        self.assertEqual(len(captured), 1)
        self.assertEqual(captured[0].get("type"), "capture_tool_call")
        expected = module.extract_paths(event)
        self.assertTrue(expected, "fixture must actually carry path-shaped keys")
        self.assertEqual(
            captured[0].get("paths"),
            expected,
            f"run_capture must forward extract_paths(event) as paths, got {captured[0]}",
        )

    def test_posttooluse_forwards_extracted_paths(self) -> None:
        module = load_adapter()
        event = {
            "hook_event_name": "PostToolUse",
            "cwd": str(ROOT),
            "session_id": "s-1",
            "tool_name": "Edit",
            "tool_input": {"file_path": "/x/y.rs"},
            "tool_response": "ok",
        }
        rc, captured = self._capture(module, event)

        self.assertEqual(rc, 0)
        self.assertEqual(len(captured), 1)
        self.assertEqual(captured[0].get("type"), "capture_tool_result")
        expected = module.extract_paths(event)
        self.assertTrue(expected, "fixture must actually carry path-shaped keys")
        self.assertEqual(
            captured[0].get("paths"),
            expected,
            f"run_capture must forward extract_paths(event) as paths, got {captured[0]}",
        )

    def test_absent_paths_key_omitted(self) -> None:
        module = load_adapter()
        # Deliberately NO cwd / path / filePath / file_path key anywhere, so
        # extract_paths(event) is genuinely [] here. (A real hook envelope
        # always carries `cwd`, itself one of extract_paths' matched
        # path-shaped keys — this fixture isolates the
        # `extract_paths(event) or None` -> strip_none forwarding mechanism,
        # not a claim that real events are ever paths-empty.)
        event = {
            "hook_event_name": "PostToolUse",
            "session_id": "s-1",
            "tool_name": "Bash",
            "tool_response": "compiled 3 crates",
        }
        self.assertEqual(module.extract_paths(event), [], "fixture must have no path-shaped keys")

        rc, captured = self._capture(module, event)

        self.assertEqual(rc, 0)
        self.assertEqual(len(captured), 1)
        self.assertNotIn(
            "paths",
            captured[0],
            "an empty extract_paths() result must be omitted (strip_none), "
            f"never sent as [], got {captured[0]}",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
