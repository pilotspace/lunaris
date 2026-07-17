#!/usr/bin/env python3
"""ADD task `staleness-pass` (contract FROZEN @ v1, engram-soul-loop task 6).

`run_capture`'s posttooluse fast-path payload (`capture_tool_result`) must
carry `"commit": True` when the event's command text
(`tool_input.command` / `toolInput.command`) matches `git commit` as a
standalone token sequence (`\\bgit\\s+commit\\b`), so the server can spawn the
shared verify-agenda sweep on a commit-shaped capture
(`.add/tasks/staleness-pass/TASK.md` §3 CONTRACT). Absent otherwise —
`strip_none` drops it rather than sending `"commit": False`.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_adapter_commit_detect.py
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


class RunCaptureDetectsCommit(unittest.TestCase):
    def setUp(self) -> None:
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

    def test_git_commit_command_sets_commit_true(self) -> None:
        module = load_adapter()
        event = {
            "hook_event_name": "PostToolUse",
            "cwd": str(ROOT),
            "session_id": "s-1",
            "tool_name": "Bash",
            "tool_input": {"command": "git commit -m x"},
            "tool_response": "ok",
        }
        rc, captured = self._capture(module, event)

        self.assertEqual(rc, 0)
        self.assertEqual(len(captured), 1)
        self.assertEqual(captured[0].get("type"), "capture_tool_result")
        self.assertIs(
            captured[0].get("commit"),
            True,
            f"a 'git commit' command must set commit: True, got {captured[0]}",
        )

    def test_git_commit_command_camelcase_tool_input_sets_commit_true(self) -> None:
        module = load_adapter()
        event = {
            "hook_event_name": "PostToolUse",
            "cwd": str(ROOT),
            "session_id": "s-1",
            "tool_name": "Bash",
            "toolInput": {"command": "git   commit --amend"},
            "tool_response": "ok",
        }
        rc, captured = self._capture(module, event)

        self.assertEqual(rc, 0)
        self.assertEqual(len(captured), 1)
        self.assertIs(
            captured[0].get("commit"),
            True,
            f"toolInput.command must also be checked, got {captured[0]}",
        )

    def test_git_log_command_omits_commit_key(self) -> None:
        module = load_adapter()
        event = {
            "hook_event_name": "PostToolUse",
            "cwd": str(ROOT),
            "session_id": "s-1",
            "tool_name": "Bash",
            "tool_input": {"command": "git log"},
            "tool_response": "ok",
        }
        rc, captured = self._capture(module, event)

        self.assertEqual(rc, 0)
        self.assertEqual(len(captured), 1)
        self.assertNotIn(
            "commit",
            captured[0],
            f"a non-commit git command must never send 'commit' (strip_none drops False), got {captured[0]}",
        )

    def test_echo_commit_command_omits_commit_key(self) -> None:
        module = load_adapter()
        event = {
            "hook_event_name": "PostToolUse",
            "cwd": str(ROOT),
            "session_id": "s-1",
            "tool_name": "Bash",
            "tool_input": {"command": "echo commit"},
            "tool_response": "ok",
        }
        rc, captured = self._capture(module, event)

        self.assertEqual(rc, 0)
        self.assertEqual(len(captured), 1)
        self.assertNotIn(
            "commit",
            captured[0],
            f"'echo commit' has no standalone 'git commit' token sequence, got {captured[0]}",
        )

    def test_non_dict_tool_input_does_not_crash(self) -> None:
        module = load_adapter()
        event = {
            "hook_event_name": "PostToolUse",
            "cwd": str(ROOT),
            "session_id": "s-1",
            "tool_name": "Bash",
            "tool_input": "git commit -m x",  # malformed shape: not a dict
            "tool_response": "ok",
        }
        rc, captured = self._capture(module, event)

        self.assertEqual(rc, 0)
        self.assertEqual(len(captured), 1)
        self.assertNotIn(
            "commit",
            captured[0],
            f"a non-dict tool_input must fail open to no commit detection, got {captured[0]}",
        )

    def test_pretooluse_never_carries_commit_key(self) -> None:
        # §3 CONTRACT scopes commit detection to run_capture's POSTTOOLUSE
        # fast path only.
        module = load_adapter()
        event = {
            "hook_event_name": "PreToolUse",
            "cwd": str(ROOT),
            "session_id": "s-1",
            "tool_name": "Bash",
            "tool_input": {"command": "git commit -m x"},
        }
        rc, captured = self._capture(module, event)

        self.assertEqual(rc, 0)
        self.assertEqual(len(captured), 1)
        self.assertEqual(captured[0].get("type"), "capture_tool_call")
        self.assertNotIn("commit", captured[0])


if __name__ == "__main__":
    unittest.main(verbosity=2)
