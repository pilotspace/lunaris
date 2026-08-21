#!/usr/bin/env python3
"""ADD task `context-savings-telemetry` (contract FROZEN @ v1, engram-soul-loop
task 10) — pure aggregation + read-only surface guard for
`scripts/context-savings-report.py`.

`aggregate(episodes: list[dict]) -> dict` is the pure, Moon-free core: it
takes already-parsed episode docs (the `{"source": ..., "metadata": {...}}`
shape every `lunaris:*` capture writes — see
`crates/lunaris-hook/src/context.rs::find_turn_feedback_metadata` for the
same shape read back in Rust) and sums the `lunaris:memory_injection` /
`lunaris:turn_feedback` counters into one per-scope report.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_context_savings_report.py
"""

from __future__ import annotations

import importlib.util
import re
import unittest
import unittest.mock
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "context-savings-report.py"


def load_module():
    spec = importlib.util.spec_from_file_location("lunaris_context_savings_report", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class AggregateCounts(unittest.TestCase):
    """Scenario: report aggregates a scope correctly (§2 SCENARIOS)."""

    def test_aggregate_counts(self) -> None:
        module = load_module()
        episodes = [
            {"source": "lunaris:memory_injection", "metadata": {"injected_tokens_est": 100}},
            {"source": "lunaris:memory_injection", "metadata": {"injected_tokens_est": 50}},
            {
                "source": "lunaris:turn_feedback",
                "metadata": {
                    "detector": "ok",
                    "transcript_stats": {
                        "file_bytes": 1234,
                        "tool_call_count": 2,
                        "final_text_chars": 42,
                    },
                    "verdicts": [
                        {"verdict": "cited"},
                        {"verdict": "cited"},
                        {"verdict": "cited"},
                        {"verdict": "uncited"},
                    ],
                },
            },
            {
                "source": "lunaris:turn_feedback",
                "metadata": {"detector": "skipped_no_transcript", "verdicts": []},
            },
            # malformed episode docs — must be skipped, not fatal.
            "not-a-dict",
            {"source": "lunaris:turn_feedback"},  # no metadata key at all
            {"source": "lunaris:turn_feedback", "metadata": "not-a-dict-either"},
        ]

        result = module.aggregate(episodes)

        self.assertEqual(result["injected_tokens_total"], 150)
        self.assertEqual(result["injection_count"], 2)
        self.assertEqual(result["turns"], 2)
        self.assertEqual(result["tool_calls"], 2)
        self.assertEqual(result["cited"], 3)
        self.assertEqual(result["uncited"], 1)
        self.assertAlmostEqual(result["cited_rate"], 0.75)

    def test_cited_rate_none_when_no_verdicts(self) -> None:
        module = load_module()
        episodes = [
            {
                "source": "lunaris:turn_feedback",
                "metadata": {"detector": "skipped_no_transcript", "verdicts": []},
            },
        ]
        result = module.aggregate(episodes)
        self.assertIsNone(result["cited_rate"])
        self.assertEqual(result["cited"], 0)
        self.assertEqual(result["uncited"], 0)

    def test_aggregate_over_empty_input(self) -> None:
        module = load_module()
        result = module.aggregate([])
        self.assertEqual(result["injected_tokens_total"], 0)
        self.assertEqual(result["injection_count"], 0)
        self.assertEqual(result["turns"], 0)
        self.assertEqual(result["tool_calls"], 0)
        self.assertEqual(result["cited"], 0)
        self.assertEqual(result["uncited"], 0)
        self.assertIsNone(result["cited_rate"])


class ModuleIsReadOnly(unittest.TestCase):
    """Scenario: report script is read-only (§2 SCENARIOS).

    Structural check (not a live-Moon proof, per the TASK.md freeze note):
    every RESP command literal the module issues via `.cmd("...")` must be
    in the read allowlist, and none may be a known write command.
    """

    READ_ALLOWLIST = {"SCAN", "GET", "MGET", "HELLO", "AUTH", "SELECT", "PING"}
    WRITE_BLOCKLIST = {
        "SET",
        "SETEX",
        "DEL",
        "UNLINK",
        "HSET",
        "HDEL",
        "EXPIRE",
        "MQ",
        "GRAPH.ADDNODE",
        "GRAPH.CREATE",
        "GRAPH.QUERY",
        "TXN",
        "FLUSHALL",
        "FLUSHDB",
        "BGSAVE",
        "BGREWRITEAOF",
        "APPEND",
        "INCR",
        "DECR",
    }

    def test_report_module_is_readonly(self) -> None:
        source = SCRIPT.read_text()
        commands = set(re.findall(r'\.cmd\(\s*"([A-Z_.]+)"', source))
        self.assertTrue(commands, "expected the module to issue at least one RESP command")
        unexpected = commands - self.READ_ALLOWLIST
        self.assertFalse(
            unexpected,
            f"scripts/context-savings-report.py issued non-read RESP commands: {unexpected}",
        )
        blocked_hits = commands & self.WRITE_BLOCKLIST
        self.assertFalse(
            blocked_hits,
            f"scripts/context-savings-report.py issued write commands: {blocked_hits}",
        )

    def test_help_runs(self) -> None:
        module = load_module()
        with self.assertRaises(SystemExit) as ctx:
            module.main(["--help"])
        self.assertEqual(ctx.exception.code, 0)


class DefaultEndpoint(unittest.TestCase):
    """--host/--port default from LUNARIS_MOON_URL (the sibling-script
    convention, e.g. bench-dialog-chat.py), else 127.0.0.1:6380."""

    def test_env_url_overrides_default(self) -> None:
        module = load_module()
        with unittest.mock.patch.dict(
            "os.environ", {"LUNARIS_MOON_URL": "moon://10.0.0.5:6381"}
        ):
            self.assertEqual(module.default_moon_endpoint(), ("10.0.0.5", 6381))

    def test_fallback_without_env(self) -> None:
        module = load_module()
        with unittest.mock.patch.dict("os.environ", {}, clear=False) as env:
            env.pop("LUNARIS_MOON_URL", None)
            self.assertEqual(module.default_moon_endpoint(), ("127.0.0.1", 6380))


if __name__ == "__main__":
    unittest.main(verbosity=2)
