"""ADD task `contextd-cold-start-lifecycle` (contract FROZEN @ v1, 2026-07-14).

Red-first suite for the three lifecycle defects proven live in the
2026-07-14 hooks deep test (memory `project_lunaris_hooks_deep_test_findings`):

1. `stop_verify_contextd` is a silent no-op — its pgrep pattern starts with
   `--`, which pgrep parses as a flag (exit 2) and `check=False` swallows.
   Every `--verify` run leaks a GGUF-loaded daemon.
2. `start_contextd` unlinks the socket of a LIVE mid-load daemon and spawns a
   duplicate — the cold-start restart storm (5 leaked daemons / 2 verifies).
3. `contextd_request` computes its deadline BEFORE the spawn, so the default
   300ms budget can never cover the first-call GGUF load — the first prompt
   after boot silently gets zero memories.

Run: python3 scripts/tests/test_contextd_lifecycle.py
Live discriminator gated by LUNARIS_VERIFY_LIVE=1 (needs Moon + GGUF weights).
"""

from __future__ import annotations

import importlib.util
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SETUP = ROOT / "scripts" / "setup-lunaris-agents.py"
ADAPTER = ROOT / "scripts" / "lunaris-codex-hook-adapter.py"


def load_module(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def spawn_socket_marker_process(sock_path: Path) -> subprocess.Popen:
    """A dummy long-lived process whose argv embeds `--socket <path>` exactly
    like a real contextd — the shape both pgrep-based helpers must match."""
    return subprocess.Popen(
        [sys.executable, "-c", "import time; time.sleep(300)", "--socket", str(sock_path)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


class StopVerifyContextdTest(unittest.TestCase):
    """§2 'stop_verify_contextd kills the socket's daemon'."""

    def test_stop_verify_contextd_kills_socket_daemon(self):
        setup = load_module(SETUP, "setup_lunaris_agents_t2")
        with tempfile.TemporaryDirectory() as td:
            sock = Path(td) / "verify.sock"
            proc = spawn_socket_marker_process(sock)
            try:
                time.sleep(0.2)
                setup.stop_verify_contextd(sock)
                deadline = time.monotonic() + 2.0
                while time.monotonic() < deadline and proc.poll() is None:
                    time.sleep(0.05)
                self.assertIsNotNone(
                    proc.poll(),
                    "stop_verify_contextd must terminate the daemon bound to its socket "
                    "(pgrep pattern starting with '--' is parsed as a pgrep FLAG and "
                    "silently matches nothing)",
                )
            finally:
                if proc.poll() is None:
                    proc.kill()


class StartContextdLivenessTest(unittest.TestCase):
    """§2 'live socket is never unlinked'."""

    def test_start_contextd_never_unlinks_live_socket(self):
        adapter = load_module(ADAPTER, "lunaris_adapter_t2")
        with tempfile.TemporaryDirectory() as td:
            sock = Path(td) / "ctx.sock"
            # A real-but-unresponsive socket file: bind, never accept — the
            # mid-GGUF-load daemon shape (health probe fails, process alive).
            server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            server.bind(str(sock))
            # NOTE: no listen() — connects hang/fail like a busy daemon.
            owner = spawn_socket_marker_process(sock)
            spawned: list[Path] = []
            adapter.spawn_contextd = lambda p: spawned.append(p)
            try:
                time.sleep(0.2)
                adapter.start_contextd(sock)
                self.assertTrue(
                    sock.exists(),
                    "start_contextd must NOT unlink the socket of a live daemon "
                    "(mid-load daemon killed by the restart storm)",
                )
                self.assertEqual(
                    spawned, [], "no duplicate daemon may be spawned while the owner lives"
                )
            finally:
                owner.kill()
                server.close()


class FakeSlowContextd(threading.Thread):
    """Unix-socket server: health answers instantly; any other request is
    answered only after `delay_s` — the first-call GGUF-load shape."""

    def __init__(self, sock_path: Path, delay_s: float):
        super().__init__(daemon=True)
        self.sock_path = sock_path
        self.delay_s = delay_s
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(str(sock_path))
        self.server.listen(8)
        self.stop_flag = False

    def run(self):
        while not self.stop_flag:
            try:
                self.server.settimeout(0.2)
                conn, _ = self.server.accept()
            except (TimeoutError, OSError):
                continue
            threading.Thread(target=self.handle, args=(conn,), daemon=True).start()

    def handle(self, conn: socket.socket):
        try:
            conn.settimeout(5)
            chunks = []
            while True:
                b = conn.recv(65536)
                if not b:
                    break
                chunks.append(b)
                if b"}" in b:
                    break
            req = json.loads(b"".join(chunks) or b"{}")
            if req.get("type") == "health":
                conn.sendall(b'{"ok":true}')
            else:
                time.sleep(self.delay_s)
                conn.sendall(
                    json.dumps(
                        {"ok": True, "rendered_context": "cold-start-proof", "memories": []}
                    ).encode()
                )
            conn.close()
        except OSError:
            pass

    def stop(self):
        self.stop_flag = True
        self.server.close()


class ColdDeadlineTest(unittest.TestCase):
    """§2 'cold start extends the deadline; warm does not'."""

    def test_cold_request_extends_deadline_warm_does_not(self):
        adapter = load_module(ADAPTER, "lunaris_adapter_t2b")
        with tempfile.TemporaryDirectory() as td:
            sock = Path(td) / "slow.sock"
            fake = FakeSlowContextd(sock, delay_s=2.5)
            fake.start()
            os.environ["LUNARIS_CONTEXTD_SOCKET"] = str(sock)
            # The fake is a stand-in for the daemon the adapter JUST spawned:
            # simulate the spawn by monkeypatching start_contextd to record the
            # cold transition without launching a real binary.
            cold_spawned = {"flag": False}

            def fake_start(path):
                cold_spawned["flag"] = True

            adapter.start_contextd = fake_start
            adapter.contextd_healthy = lambda p, timeout_ms: cold_spawned["flag"]
            try:
                # WARM path (daemon pre-existing): healthy=True immediately,
                # 300ms budget, 2.5s reply -> must return {} (semantics kept).
                cold_spawned["flag"] = True
                warm = adapter.contextd_request({"type": "recall_for_prompt"}, timeout_ms=300)
                self.assertEqual(warm, {}, "warm path must keep the caller's 300ms budget")

                # COLD path (this call spawns): the extended cold budget must
                # ride out the 2.5s first reply.
                cold_spawned["flag"] = False
                cold = adapter.contextd_request({"type": "recall_for_prompt"}, timeout_ms=300)
                self.assertEqual(
                    cold.get("rendered_context"),
                    "cold-start-proof",
                    "cold start must extend the deadline past the model-load delay "
                    f"(got {cold!r})",
                )
            finally:
                fake.stop()
                os.environ.pop("LUNARIS_CONTEXTD_SOCKET", None)


class DefaultEnvVerifyLiveTest(unittest.TestCase):
    """§2 'default-env verify passes and cleans up' — the exit-criterion
    discriminator. Gated: needs live Moon + GGUF weights."""

    def test_default_env_verify_passes_and_cleans_up(self):
        if os.environ.get("LUNARIS_VERIFY_LIVE") != "1":
            self.skipTest("LUNARIS_VERIFY_LIVE not set (live gate)")
        moon_url = os.environ.get("LUNARIS_VERIFY_MOON_URL", "moon://127.0.0.1:6399")
        env = {
            k: v
            for k, v in os.environ.items()
            if not k.startswith("LUNARIS_CONTEXT_TIMEOUT")
            and not k.startswith("LUNARIS_CONTEXT_CAPTURE_TIMEOUT")
        }
        with tempfile.TemporaryDirectory() as td:
            settings = Path(td) / "settings.json"
            settings.write_text("{}")
            setup = subprocess.run(
                [
                    sys.executable,
                    str(SETUP),
                    "--agent",
                    "claude",
                    "--runner",
                    "local",
                    "--no-build-hooks",
                    "--claude-settings",
                    str(settings),
                    "--moon-url",
                    moon_url,
                ],
                capture_output=True,
                text=True,
                env=env,
                timeout=300,
            )
            self.assertEqual(setup.returncode, 0, f"setup failed: {setup.stderr[-500:]}")
            verify = subprocess.run(
                [
                    sys.executable,
                    str(SETUP),
                    "--agent",
                    "claude",
                    "--verify",
                    "--claude-settings",
                    str(settings),
                    "--moon-url",
                    moon_url,
                    "--no-moon-autostart",
                ],
                capture_output=True,
                text=True,
                env=env,
                timeout=600,
            )
            out = verify.stdout + verify.stderr
            self.assertIn("VERIFY PASS: capture", out)
            self.assertIn(
                "VERIFY PASS: cross-session inject",
                out,
                f"default-env verify must pass without timeout overrides; output: {out[-600:]}",
            )
            self.assertEqual(verify.returncode, 0)
            leftovers = subprocess.run(
                ["pgrep", "-f", "lunaris-contextd"], capture_output=True, text=True
            )
            strays = [
                line
                for line in leftovers.stdout.splitlines()
                if line.strip()
                and "lunaris-verify-" in subprocess.run(
                    ["ps", "-o", "args=", "-p", line.strip()],
                    capture_output=True,
                    text=True,
                ).stdout
            ]
            self.assertEqual(strays, [], f"verify leaked contextd daemons: {strays}")


if __name__ == "__main__":
    unittest.main(verbosity=2)
