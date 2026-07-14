#!/usr/bin/env python3
"""setup-lunaris-agents.py must MERGE its Claude hooks, never clobber.

Regression guard (2026-07-14): `update_claude` replaced each hook event's
array wholesale (`data["hooks"][event] = [...]`). On a machine that already
had hooks (GSD guards, ruff/eslint), installing Lunaris silently wiped them.
The setup step must instead:
  1. preserve every pre-existing non-Lunaris hook,
  2. add the Lunaris hooks alongside them,
  3. be idempotent (re-running adds no duplicate Lunaris groups),
  4. carry a timeout on every Lunaris command (contextd cold-start must not
     block the CLI forever),
  5. on `--hooks off`, remove only the Lunaris hooks and leave the rest.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_hooks_merge_safe.py
"""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
SETUP = ROOT / "scripts" / "setup-lunaris-agents.py"
GSD_MARKER = "gsd-context-monitor.js"
LUNARIS_MARKER = "lunaris-codex-hook-adapter.py"


def load_setup():
    spec = importlib.util.spec_from_file_location("setup_lunaris_agents", SETUP)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def make_args(hooks: str = "on"):
    return SimpleNamespace(
        scope="test-scope",
        hooks=hooks,
        dry_run=False,
        storage_url="moon://127.0.0.1:6381",
        storage_backend="moon",
        moon_url="moon://127.0.0.1:6381",
        moon_full_features=True,
        runner="local",
    )


def seed_settings(path: Path) -> None:
    """A settings.json that already has a GSD PostToolUse guard + PreToolUse."""
    path.write_text(
        json.dumps(
            {
                "hooks": {
                    "PostToolUse": [
                        {
                            "matcher": "Bash|Edit|Write",
                            "hooks": [
                                {"type": "command", "command": f"node {GSD_MARKER}", "timeout": 10}
                            ],
                        }
                    ],
                    "PreToolUse": [
                        {
                            "matcher": "*",
                            "hooks": [{"type": "command", "command": "echo pre"}],
                        }
                    ],
                },
                "mcpServers": {"other": {"command": "x", "args": []}},
            },
            indent=2,
        )
    )


def all_commands(groups: list) -> list[str]:
    return [h.get("command", "") for g in groups for h in g.get("hooks", [])]


class HooksMergeSafe(unittest.TestCase):
    def _install(self, module, path: Path, hooks: str = "on"):
        mcp = {"command": "lunaris-mcp", "args": []}
        module.update_claude(path, make_args(hooks), mcp)
        return json.loads(path.read_text())

    def test_preserves_existing_gsd_hooks(self) -> None:
        module = load_setup()
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "settings.json"
            seed_settings(path)
            data = self._install(module, path)

            post = data["hooks"]["PostToolUse"]
            cmds = all_commands(post)
            self.assertTrue(
                any(GSD_MARKER in c for c in cmds),
                f"GSD PostToolUse hook must survive install; got {cmds}",
            )
            self.assertTrue(
                any(LUNARIS_MARKER in c for c in cmds),
                f"Lunaris post-tool hook must be added; got {cmds}",
            )
            pre_cmds = all_commands(data["hooks"]["PreToolUse"])
            self.assertIn("echo pre", pre_cmds, "pre-existing PreToolUse hook must survive")
            self.assertIn("other", data["mcpServers"], "pre-existing MCP server must survive")

    def test_lunaris_hooks_carry_timeout(self) -> None:
        module = load_setup()
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "settings.json"
            seed_settings(path)
            data = self._install(module, path)
            lun = [
                h
                for g in data["hooks"]["UserPromptSubmit"]
                for h in g.get("hooks", [])
                if LUNARIS_MARKER in h.get("command", "")
            ]
            self.assertTrue(lun, "UserPromptSubmit must have Lunaris hooks")
            for h in lun:
                self.assertIn("timeout", h, f"Lunaris hook needs a timeout: {h}")
                self.assertGreater(h["timeout"], 0)

    def test_idempotent_no_duplicate_lunaris_groups(self) -> None:
        module = load_setup()
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "settings.json"
            seed_settings(path)
            self._install(module, path)
            data = self._install(module, path)  # second run
            for event in ("UserPromptSubmit", "PostToolUse"):
                groups = data["hooks"][event]
                lun_groups = [
                    g
                    for g in groups
                    if any(LUNARIS_MARKER in h.get("command", "") for h in g.get("hooks", []))
                ]
                self.assertEqual(
                    len(lun_groups), 1, f"{event}: exactly one Lunaris group, got {len(lun_groups)}"
                )
            # GSD still present exactly once
            post_cmds = all_commands(data["hooks"]["PostToolUse"])
            self.assertEqual(
                sum(GSD_MARKER in c for c in post_cmds), 1, "GSD hook must not duplicate or vanish"
            )

    def test_hooks_off_removes_only_lunaris(self) -> None:
        module = load_setup()
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "settings.json"
            seed_settings(path)
            self._install(module, path, hooks="on")
            data = self._install(module, path, hooks="off")
            post_cmds = all_commands(data["hooks"].get("PostToolUse", []))
            self.assertTrue(any(GSD_MARKER in c for c in post_cmds), "GSD survives --hooks off")
            self.assertFalse(
                any(LUNARIS_MARKER in c for c in post_cmds),
                f"Lunaris hooks removed on --hooks off; got {post_cmds}",
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
