"""Pytest conftest — Lunaris Python bindings test harness.

Provides a `moon_backend_url` fixture that returns the Moon URL to test
against, OR skips the test when no backend is reachable. This lets the
offline DSL / toggle / GIL-discipline tests run anywhere while the
full open/ingest/recall suite only runs in a CI environment (or dev box)
that has Moon listening.

Environment variables:

- `LUNARIS_TEST_MOON_URL` — override the default `moon://127.0.0.1:6379`.
- `LUNARIS_TEST_POSTGRES_URL` — optional Postgres test target.
"""
from __future__ import annotations

import asyncio
import os
import socket
from typing import Optional

import pytest

DEFAULT_MOON_URL = "moon://127.0.0.1:6379"


def _host_port_reachable(host: str, port: int, timeout_s: float = 0.4) -> bool:
    """Cheap synchronous TCP reachability probe."""
    try:
        with socket.create_connection((host, port), timeout=timeout_s):
            return True
    except (OSError, ValueError):
        return False


def _parse_moon_host_port(url: str) -> Optional[tuple[str, int]]:
    if not url.startswith("moon://"):
        return None
    rest = url[len("moon://"):]
    host_port = rest.split("/")[0].split("?")[0]
    if ":" not in host_port:
        return host_port, 6379
    host, port_str = host_port.split(":", 1)
    try:
        return host, int(port_str)
    except ValueError:
        return None


@pytest.fixture
def moon_backend_url() -> str:
    """Skip the test unless a functional Moon backend is reachable.

    Two-stage probe:

    1. **TCP reachability** — cheap `connect()` to the host:port from the URL.
    2. **Lunaris handshake** — attempt `await lunaris.open(url)` once per
       session (cached). Plain Redis listening on 6379 (no RediSearch module)
       passes TCP reachability but fails the Lunaris index bootstrap with
       `FT.CREATE` — skip with the exact error string so the operator knows
       to swap to a real Moon dev box.

    Returns the URL string so the test can pass it straight to
    `lunaris.open(url)`. Use this fixture in any test that needs a live
    storage backend; offline tests (DSL parity, from_env / from_config
    resolution, GIL discipline on the URL-parse error path) should NOT
    depend on it.
    """
    url = os.environ.get("LUNARIS_TEST_MOON_URL", DEFAULT_MOON_URL)
    parsed = _parse_moon_host_port(url)
    if parsed is None:
        pytest.skip(f"malformed Moon URL: {url!r}")
    host, port = parsed
    if not _host_port_reachable(host, port):
        pytest.skip(
            f"Moon backend not reachable at {host}:{port}; set "
            f"LUNARIS_TEST_MOON_URL or start a Moon dev box to run this test"
        )
    # Stage 2 — attempt a real handshake to distinguish Moon from plain Redis.
    if _handshake_cache.get(url) is False:
        pytest.skip(_handshake_cache["__reason__"])
    if _handshake_cache.get(url) is None:
        _probe_handshake(url)
        if _handshake_cache.get(url) is False:
            pytest.skip(_handshake_cache["__reason__"])
    return url


_handshake_cache: dict = {}


def _probe_handshake(url: str) -> None:
    """One-shot probe: attempt `lunaris.open(url)` and cache the result so
    subsequent tests don't pay the probe cost."""
    import lunaris

    async def run():
        try:
            await lunaris.open(url)
            _handshake_cache[url] = True
        except Exception as e:
            _handshake_cache[url] = False
            _handshake_cache["__reason__"] = (
                f"Lunaris handshake failed for {url!r}: {type(e).__name__}: {e}"
            )

    asyncio.run(run())


# Ensure pytest-asyncio runs every `async def test_...` automatically;
# mirrors the pyproject.toml [tool.pytest.ini_options] asyncio_mode setting
# so `uv run pytest` from inside the venv works even if the user passes
# -p no:cacheprovider (which can strip config sections).
@pytest.fixture(scope="session")
def event_loop_policy() -> asyncio.AbstractEventLoopPolicy:
    return asyncio.get_event_loop_policy()
