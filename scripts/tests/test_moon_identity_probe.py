#!/usr/bin/env python3
"""Hygiene fix (2026-07-17): installer default port + Moon-identity probe.

Red-first suite for two live findings from the 2026-07-14 hooks deep test
(re-verified 2026-07-17):

  - DEFAULT_MOON_URL pointed at 6380 — on the reference box that port is an
    ai-proxy Redis, NOT Moon (the launchd Moon agent listens on 6381). A
    default install silently wrote memory into the wrong Redis.
  - `ensure_moon_running` accepted ANY TCP listener as "Moon reachable"
    (`tcp_listening` only). A foreign Redis answers RESP PING too, so
    liveness alone cannot discriminate; the probe must require a Moon-native
    command. `FT._LIST` is read-only, arity 1, and unknown to plain Redis.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_moon_identity_probe.py
"""

from __future__ import annotations

import argparse
import importlib.util
import socket
import threading
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SETUP = ROOT / "scripts" / "setup-lunaris-agents.py"


def load_setup_module():
    spec = importlib.util.spec_from_file_location("setup_lunaris_agents", SETUP)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class FakeRespServer:
    """Minimal RESP endpoint: +PONG to PING; configurable FT._LIST reply.

    `moon=False` mimics a plain/proxy Redis (PING works, FT._LIST is an
    unknown command). `moon=True` mimics Moon (FT._LIST answers an array).
    """

    def __init__(self, moon: bool) -> None:
        self.moon = moon
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(4)
        self.port = self.sock.getsockname()[1]
        self._stop = threading.Event()
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()

    def _serve(self) -> None:
        while not self._stop.is_set():
            try:
                self.sock.settimeout(0.2)
                conn, _ = self.sock.accept()
            except OSError:
                continue
            with conn:
                conn.settimeout(2.0)
                try:
                    while True:
                        data = conn.recv(1024)
                        if not data:
                            break
                        if b"PING" in data.upper():
                            conn.sendall(b"+PONG\r\n")
                        elif b"FT._LIST" in data.upper():
                            if self.moon:
                                conn.sendall(b"*0\r\n")
                            else:
                                conn.sendall(
                                    b"-ERR unknown command 'FT._LIST'\r\n"
                                )
                        else:
                            conn.sendall(b"-ERR unknown command\r\n")
                except OSError:
                    pass

    def close(self) -> None:
        self._stop.set()
        try:
            self.sock.close()
        except OSError:
            pass
        self.thread.join(timeout=2.0)


def probe_args(module) -> argparse.Namespace:
    # Non-explicit --moon-bin (equals the vendored default) so
    # resolve_moon_bin never hard-exits; autostart off — the port under test
    # is already occupied by the fake endpoint.
    return argparse.Namespace(
        moon_bin=str(module.MOON_BIN),
        moon_autostart=False,
    )


class DefaultPort(unittest.TestCase):
    """The shipped default must be the Moon launchd port, not 6380."""

    def test_default_moon_url_is_6381(self) -> None:
        module = load_setup_module()
        self.assertTrue(
            module.DEFAULT_MOON_URL.endswith(":6381"),
            f"DEFAULT_MOON_URL={module.DEFAULT_MOON_URL!r} — 6380 is the "
            "ai-proxy Redis on the reference box; Moon's launchd agent is 6381",
        )

    def test_parse_fallback_port_is_6381(self) -> None:
        module = load_setup_module()
        self.assertEqual(module.parse_moon_host_port("moon://127.0.0.1"), ("127.0.0.1", 6381))
        self.assertEqual(
            module.parse_moon_host_port("moon://127.0.0.1:not-a-port"),
            ("127.0.0.1", 6381),
        )


class MoonIdentityProbe(unittest.TestCase):
    """A reachable port is not enough — the endpoint must speak Moon."""

    def test_foreign_redis_is_rejected_loudly(self) -> None:
        module = load_setup_module()
        server = FakeRespServer(moon=False)
        try:
            detail = module.ensure_moon_running(
                probe_args(module), f"moon://127.0.0.1:{server.port}"
            )
        finally:
            server.close()
        self.assertIsNotNone(
            detail,
            "a PONG-answering endpoint that rejects FT._LIST is NOT Moon — "
            "ensure_moon_running must fail instead of writing memory into a "
            "foreign Redis",
        )
        self.assertIn("FT._LIST", detail)

    def test_real_moon_endpoint_passes(self) -> None:
        module = load_setup_module()
        server = FakeRespServer(moon=True)
        try:
            detail = module.ensure_moon_running(
                probe_args(module), f"moon://127.0.0.1:{server.port}"
            )
        finally:
            server.close()
        self.assertIsNone(detail)

    def test_non_moon_scheme_skips_probe(self) -> None:
        module = load_setup_module()
        self.assertIsNone(
            module.ensure_moon_running(probe_args(module), "sqlite:///tmp/x.db")
        )


if __name__ == "__main__":
    unittest.main()
