#!/usr/bin/env python3
"""ADD task `contextd-scope-forwarding` (contract FROZEN @ v1, 2026-07-14).

P0 scope-isolation bug, live-repro'd 2026-07-14: the adapter's contextd
socket requests never carry the caller's LUNARIS_HOOK_SCOPE, so a
long-lived contextd applies ITS OWN inherited env scope to every request —
216 episodes from one project landed in another project's scope, and
inject recalls searched the wrong partition.

Fix under test: every contextd request payload forwards the caller's scope
(ContextRequest already accepts it; resolve_scope already prefers it).

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_contextd_scope_forwarding.py

The live cross-scope discriminator is gated on LUNARIS_VERIFY_LIVE=1 and
needs target/release/lunaris-contextd plus a live Moon at
LUNARIS_VERIFY_MOON_URL (default moon://127.0.0.1:6381).
"""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
ADAPTER = ROOT / "scripts" / "lunaris-codex-hook-adapter.py"
CONTEXTD_BIN = ROOT / "target" / "release" / "lunaris-contextd"


def load_adapter():
    spec = importlib.util.spec_from_file_location("lunaris_codex_hook_adapter", ADAPTER)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def drive_all_builders(module, captured: list[dict]) -> None:
    """Run every contextd-socket-using entry point with request capture."""
    event = {
        "hook_event_name": "PostToolUse",
        "session_id": "scope-fwd-test",
        "tool_name": "Bash",
        "tool_input": {"command": "echo hi"},
        "tool_response": {"output": "hi"},
        "cwd": "/tmp",
    }

    def fake_request(payload, **kwargs):
        captured.append(dict(payload))
        return {}

    with mock.patch.object(module, "contextd_request", side_effect=fake_request):
        pre = dict(event, hook_event_name="PreToolUse")
        module.run_capture(pre)
        module.run_capture(event)
        module.run_prompt_injection(
            {"hook_event_name": "UserPromptSubmit", "session_id": "s", "prompt": "q", "cwd": "/tmp"},
            "claude",
        )
        module.run_post_tool_injection(event, "claude")
        module.run_feedback({"hook_event_name": "Stop", "session_id": "s", "cwd": "/tmp"})


class ScopeForwarding(unittest.TestCase):
    def test_all_socket_payloads_carry_env_scope(self) -> None:
        module = load_adapter()
        captured: list[dict] = []
        with mock.patch.dict(os.environ, {"LUNARIS_HOOK_SCOPE": "proj-a", "LUNARIS_CONTEXT_CAPTURE_FAST": "1"}):
            drive_all_builders(module, captured)
        self.assertGreaterEqual(len(captured), 5, f"expected >=5 socket requests, got {len(captured)}")
        for payload in captured:
            self.assertEqual(
                payload.get("scope"),
                "proj-a",
                f"payload type={payload.get('type')} must carry the caller scope: {payload}",
            )

    def test_unset_or_empty_env_omits_scope(self) -> None:
        module = load_adapter()
        for env in ({}, {"LUNARIS_HOOK_SCOPE": ""}):
            captured: list[dict] = []
            clean = {k: v for k, v in os.environ.items() if k != "LUNARIS_HOOK_SCOPE"}
            clean.update(env)
            clean["LUNARIS_CONTEXT_CAPTURE_FAST"] = "1"
            with mock.patch.dict(os.environ, clean, clear=True):
                drive_all_builders(module, captured)
            for payload in captured:
                self.assertNotIn(
                    "scope",
                    payload,
                    f"unset/empty env must omit scope (never send \"\"): {payload}",
                )


class LiveCrossScopeIsolation(unittest.TestCase):
    """The discriminator: a daemon born under scope-a must not swallow scope-b."""

    def test_live_cross_scope_isolation(self) -> None:
        if os.environ.get("LUNARIS_VERIFY_LIVE") != "1":
            self.skipTest("LUNARIS_VERIFY_LIVE not set")
        if not CONTEXTD_BIN.exists():
            self.skipTest("lunaris-contextd release binary not built")
        moon_url = os.environ.get("LUNARIS_VERIFY_MOON_URL", "moon://127.0.0.1:6381")
        run_id = uuid.uuid4().hex[:8]
        scope_a, scope_b = f"fwd-a-{run_id}", f"fwd-b-{run_id}"

        with tempfile.TemporaryDirectory() as tmp:
            socket_path = Path(tmp) / "fwd.sock"
            base_env = os.environ.copy()
            base_env.update(
                {
                    "LUNARIS_CONTEXTD_SOCKET": str(socket_path),
                    "LUNARIS_STORE_URL": moon_url,
                    "LUNARIS_CONTEXT_CAPTURE_FAST": "1",
                }
            )
            # Daemon inherits scope A at birth — the contamination vector.
            daemon_env = dict(base_env, LUNARIS_HOOK_SCOPE=scope_a)
            daemon = subprocess.Popen(
                [str(CONTEXTD_BIN), "--socket", str(socket_path)],
                env=daemon_env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            try:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not socket_path.exists():
                    time.sleep(0.1)
                self.assertTrue(socket_path.exists(), "daemon did not open its socket")

                # Caller runs under scope B.
                caller_env = dict(base_env, LUNARIS_HOOK_SCOPE=scope_b)
                marker = f"isolation-marker-{run_id}"
                event = {
                    "hook_event_name": "PostToolUse",
                    "session_id": "fwd-b",
                    "tool_name": "Bash",
                    "tool_input": {"command": "probe"},
                    "output": marker,
                    "tool_response": {"output": marker},
                    "cwd": "/tmp",
                }
                proc = subprocess.run(
                    [sys.executable, str(ADAPTER), "--target", "claude", "--mode", "capture"],
                    input=json.dumps(event),
                    capture_output=True,
                    text=True,
                    env=caller_env,
                    timeout=60,
                )
                self.assertEqual(proc.returncode, 0, f"capture failed: {proc.stderr}")

                # Poll Moon for where the episode landed.
                host_port = moon_url.split("://", 1)[1]
                host, port = host_port.rsplit(":", 1)

                def scan(scope: str) -> int:
                    out = subprocess.run(
                        ["redis-cli", "-h", host, "-p", port, "--scan", "--pattern", f"lunaris:{scope}:episode:*"],
                        capture_output=True,
                        text=True,
                        timeout=10,
                    ).stdout.strip()
                    return len([line for line in out.splitlines() if line])

                deadline = time.monotonic() + 15
                b_count = 0
                while time.monotonic() < deadline and b_count == 0:
                    b_count = scan(scope_b)
                    if b_count == 0:
                        time.sleep(0.5)
                a_count = scan(scope_a)
                self.assertGreater(b_count, 0, "caller-scope episode never appeared under scope B")
                self.assertEqual(a_count, 0, "daemon birth scope A must stay empty (cross-scope bleed)")
            finally:
                daemon.terminate()
                try:
                    daemon.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                for scope in (scope_a, scope_b):
                    subprocess.run(
                        f"redis-cli -h {host} -p {port} --scan --pattern 'lunaris:{scope}:*' | xargs -I{{}} redis-cli -h {host} -p {port} DEL {{}}",
                        shell=True,
                        capture_output=True,
                        timeout=15,
                    )


if __name__ == "__main__":
    unittest.main(verbosity=2)
