#!/usr/bin/env python3
"""ADD task `citation-detector` (contract FROZEN @ v1, engram-soul-loop task 3).

`run_feedback` must forward the Stop event's `transcript_path` verbatim into
the `turn_feedback` socket request, and must STOP reading the raw event's
`injected_memory_ids` key — the server now derives verdicts from the
transcript itself (`.add/tasks/citation-detector/TASK.md` §3 CONTRACT). The
old adapter read `event.get("injected_memory_ids")`, a key the Stop payload
never actually carries in production (dead code, per §0 GROUND), and never
forwarded `transcript_path` at all.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_adapter_feedback_transcript.py
"""

from __future__ import annotations

import importlib.util
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


class RunFeedbackForwardsTranscriptPath(unittest.TestCase):
    def test_transcript_path_forwarded_verbatim(self) -> None:
        module = load_adapter()
        captured: list[dict] = []

        def fake_request(payload, **kwargs):
            captured.append(dict(payload))
            return {}

        event = {
            "hook_event_name": "Stop",
            "session_id": "s-1",
            "cwd": "/tmp",
            "transcript_path": "/tmp/some-transcript.jsonl",
        }
        with mock.patch.object(module, "contextd_request", side_effect=fake_request):
            module.run_feedback(event)

        self.assertEqual(len(captured), 1)
        self.assertEqual(
            captured[0].get("transcript_path"),
            "/tmp/some-transcript.jsonl",
            f"run_feedback must forward transcript_path verbatim, got {captured[0]}",
        )

    def test_missing_transcript_path_omits_key(self) -> None:
        module = load_adapter()
        captured: list[dict] = []

        def fake_request(payload, **kwargs):
            captured.append(dict(payload))
            return {}

        event = {"hook_event_name": "Stop", "session_id": "s-1", "cwd": "/tmp"}
        with mock.patch.object(module, "contextd_request", side_effect=fake_request):
            module.run_feedback(event)

        self.assertEqual(len(captured), 1)
        self.assertNotIn(
            "transcript_path",
            captured[0],
            "an absent transcript_path must be omitted (strip_none), never sent as null",
        )

    def test_injected_memory_ids_raw_event_read_is_dropped(self) -> None:
        """The old dead-code read of `event["injected_memory_ids"]` must be
        gone — the server derives verdicts from the transcript now. Proof:
        even when the raw event carries an `injected_memory_ids` list, the
        outgoing socket payload must NOT echo it back from the raw event."""
        module = load_adapter()
        captured: list[dict] = []

        def fake_request(payload, **kwargs):
            captured.append(dict(payload))
            return {}

        event = {
            "hook_event_name": "Stop",
            "session_id": "s-1",
            "cwd": "/tmp",
            "injected_memory_ids": ["01HXPOISON0000000000000001"],
        }
        with mock.patch.object(module, "contextd_request", side_effect=fake_request):
            module.run_feedback(event)

        self.assertEqual(len(captured), 1)
        self.assertNotIn(
            "injected_memory_ids",
            captured[0],
            f"run_feedback must no longer read/forward the raw event's "
            f"injected_memory_ids, got {captured[0]}",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
