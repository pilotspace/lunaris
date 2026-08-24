"""Pytest conftest — Lunaris Python bindings test harness.

Provides a `moon_backend_url` fixture that returns the Moon URL to test
against, OR skips the test when no backend is reachable. This lets the
offline DSL / toggle / GIL-discipline tests run anywhere while the
full open/ingest/recall suite only runs in a CI environment (or dev box)
that has Moon listening.

Environment variables:

- `LUNARIS_MOON_URL` — override the default `moon://127.0.0.1:6380`.
- `LUNARIS_TEST_POSTGRES_URL` — optional Postgres test target.
"""
from __future__ import annotations

import asyncio
import os
import secrets
import socket
import time
from typing import Optional

import pytest

DEFAULT_MOON_URL = "moon://127.0.0.1:6380"


def run_tag() -> str:
    """A per-run discriminator, so a second run cannot read the first's rows.

    Any suite that writes under a source prefix or a scope MUST mix this in.
    A constant prefix makes the suite correct only against a fresh backend:
    it re-ingests its fixtures on top of the previous run's, and an
    exact-count assertion then reads double. CI gets away with it because
    runners are fresh; a developer running twice does not (F34 —
    `test_documentary_parity.py` read 12 where it wanted 6).

    `test_conversational_parity.py` had this right from the start with a
    private copy; it now imports this one, so there is a single definition to
    find when the next suite needs it.
    """
    return f"{int(time.time() * 1000):x}{secrets.token_hex(3)}"


def run_window_offset_ms() -> int:
    """A per-run shift for any fixture whose test filters on VALID TIME.

    `run_tag` is not enough on its own. A per-run source prefix keeps two runs'
    rows distinguishable, but the recipes filter that prefix in memory AFTER a
    global `top(30)` — while `.between()` pushes `@valid_time:[lo hi]` down to
    Moon. So every run's rows land in the SAME window and compete for the same
    30 slots. Measured: run 2 of the timeline scenario returned 12, run 5
    returned exactly 30 (the cap), and once the TS suite piled onto the same
    window a later run returned 5 of its own 6 — an UNDER-return, which reads
    like a product bug rather than a dirty store.

    Shifting the window per run is what actually isolates: Moon's own numeric
    filter then excludes the other runs, so the top-30 never sees them.

    Drawn from entropy, not from the clock. A time-derived offset looks tidier
    but two suites that start in the same second — which is exactly what a
    `pytest tests/` or a CI matrix does — would land on the same window and
    reproduce the bug this exists to prevent. The shift is a uniform
    translation, so it changes no assertion's meaning; the only thing at stake
    is collision probability, and 1e5 seven-day slots puts that at ~1e-5 per
    pair of runs. A collision just re-creates the old failure, loudly.
    """
    return secrets.randbelow(100_000) * 7 * 86_400_000


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
    url = os.environ.get("LUNARIS_MOON_URL", DEFAULT_MOON_URL)
    parsed = _parse_moon_host_port(url)
    if parsed is None:
        pytest.skip(f"malformed Moon URL: {url!r}")
    host, port = parsed
    if not _host_port_reachable(host, port):
        pytest.skip(
            f"Moon backend not reachable at {host}:{port}; set "
            f"LUNARIS_MOON_URL or start a Moon dev box to run this test"
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
