"""Plan 08-04 — per-driver backend parity (Python driver).

Python-driver entry for the Phase 8 success-criterion #4 test: within a
single process, ingest the FixtureCorpus via lunaris-py's
`conformance_fixture_episodes` + `handle.ingest` into Moon, then scan via
`scan_kv_prefix` and assert the normalized shape matches the committed
golden reference at
`crates/lunaris-conformance/fixtures/golden/bindings_fixture.json`.

Through 0.6.x this ran against BOTH Moon and Postgres; 0.7.0 deleted the
second backend, so the surviving parity axis is across the three language
drivers, not across substrates.

The Rust and TypeScript drivers (`crates/lunaris-conformance/tests/
run_bindings_backend_parity.rs` and `crates/lunaris-ts/__test__/
backend_parity.spec.mts`) replicate this exact flow in their languages.
Each driver's assertion is INDEPENDENT — no `assert rust_rows == py_rows`
or equivalent is made in any of the three test bodies. Per-driver
backend parity is the correct interpretation of success criterion #4
(see `.planning/phases/08-sdk-bindings/08-04-PLAN.md` revision
iteration 2 scope-reset note).

Requires the `bindings-it` feature build (`maturin develop --release
--features bindings-it`). When `LUNARIS_MOON_URL` is unset or the Moon
it names is unreachable, the test skips neutrally — mirroring the Plan
04-03 / 05-02 two-tier skip pattern.
"""
from __future__ import annotations

import asyncio
import json
import os
import pathlib
import socket
import sys

import pytest

# Resolve the committed golden JSON relative to this test file. The
# structure is:
#   crates/lunaris-py/tests/test_backend_parity.py  (this file)
#   crates/lunaris-conformance/fixtures/golden/bindings_fixture.json
# so we walk up 3 levels (tests → lunaris-py → crates → repo root) and
# then descend into the conformance crate.
_GOLDEN_PATH = (
    pathlib.Path(__file__).resolve().parents[2]
    / "lunaris-conformance"
    / "fixtures"
    / "golden"
    / "bindings_fixture.json"
)


def _load_golden() -> dict:
    if not _GOLDEN_PATH.exists():
        pytest.skip(
            f"golden reference missing at {_GOLDEN_PATH} — run from the "
            f"lunaris repo checkout (submodule-init or re-clone if needed)"
        )
    return json.loads(_GOLDEN_PATH.read_text())


# Canonical Rust constants — must match `lunaris_conformance::fixtures::SEED`
# + `EPISODE_COUNT`. The `conformance_fixture_episodes` FFI asserts these
# against the Rust constants at the boundary so silent cross-language
# drift surfaces as a hard panic.
SEED = 0xCAFE_F00D
COUNT = 10


def _parse_host_port(url: str) -> tuple[str, int] | None:
    """Extract host:port from a `moon://` URL for the TCP probe.

    `moon://` is the only scheme `lunaris.open` accepts from 0.7.0 on;
    anything else returns None so the caller skips instead of probing a
    port nothing will ever be opened against.
    """
    scheme, default_port = "moon://", 6379
    if not url.startswith(scheme):
        return None
    rest = url[len(scheme):]
    # A store URL can carry userinfo — strip it before splitting.
    if "@" in rest:
        rest = rest.rsplit("@", 1)[1]
    authority = rest.split("/")[0].split("?")[0]
    if ":" in authority:
        host, port_str = authority.split(":", 1)
        try:
            return host, int(port_str)
        except ValueError:
            return None
    return authority, default_port


def _host_port_reachable(host: str, port: int, timeout_s: float = 1.0) -> bool:
    """Cheap TCP reachability probe. Mirror of Plan 04-03 probe_backend."""
    try:
        with socket.create_connection((host, port), timeout=timeout_s):
            return True
    except (OSError, ValueError):
        return False


def _probe_backend(env_name: str) -> str | None:
    """Two-tier probe: env value must be set + TCP must be reachable.

    Returns the URL string to use, or None to skip. Prints the skip
    reason to stderr so the CI log surfaces the actual reason.
    """
    url = os.environ.get(env_name)
    if not url:
        print(
            f"test_backend_parity: SKIP {env_name} (env var unset)",
            file=sys.stderr,
        )
        return None
    parsed = _parse_host_port(url)
    if parsed is None:
        print(
            f"test_backend_parity: SKIP {env_name} (unknown URL scheme or malformed)",
            file=sys.stderr,
        )
        return None
    host, port = parsed
    if not _host_port_reachable(host, port):
        # Intentionally do NOT log the full URL — a store URL can carry
        # credentials in the userinfo segment (T-05-02-01 mitigation,
        # mirror of Rust-side probe_backend).
        print(
            f"test_backend_parity: SKIP {env_name} (TCP probe to {host}:{port} failed)",
            file=sys.stderr,
        )
        return None
    return url


async def _ingest_and_normalize(url: str, golden: dict) -> dict:
    """Ingest FixtureCorpus + scan + normalize into structural shape.

    Returns `{total_rows, keys_prefix, distinct_episode_ids}` matching
    the Rust-side `NormalizedRows` shape. The caller then compares
    this against the golden reference.
    """
    import lunaris

    # `conformance_fixture_episodes` asserts the SEED + COUNT match the
    # Rust constants; drift raises a hard panic at the FFI boundary.
    episodes = lunaris.lunaris.conformance_fixture_episodes(SEED, COUNT)
    assert len(episodes) == COUNT, (
        f"expected {COUNT} episodes from FFI, got {len(episodes)}"
    )

    handle = await lunaris.open(url)
    for ep in episodes:
        await handle.ingest(ep)

    prefix_bytes = golden["keys_prefix"].encode("utf-8")
    rows = await lunaris.lunaris.scan_kv_prefix(handle, prefix_bytes)

    # Parse each key as "{prefix}{ulid}" and collect unique suffixes.
    prefix_str = golden["keys_prefix"]
    distinct: set[str] = set()
    for k, _v in rows:
        try:
            key_str = bytes(k).decode("utf-8")
        except UnicodeDecodeError:
            continue
        if key_str.startswith(prefix_str):
            distinct.add(key_str[len(prefix_str):])

    return {
        "total_rows": len(rows),
        "keys_prefix": golden["keys_prefix"],
        "distinct_episode_ids": len(distinct),
    }


def _assert_structural_eq(rows: dict, golden: dict, label: str) -> None:
    """Mirror of `lunaris_conformance::bindings::assert_structural_eq`.

    Fails with a human-readable reason naming the backend ("moon
    backend failed structural parity") so CI logs surface the cause.
    """
    assert rows["keys_prefix"] == golden["keys_prefix"], (
        f"{label} prefix drift: got {rows['keys_prefix']!r} "
        f"want {golden['keys_prefix']!r}"
    )
    assert rows["distinct_episode_ids"] == golden["episode_count"], (
        f"{label} episode count drift: got {rows['distinct_episode_ids']} "
        f"want {golden['episode_count']}"
    )
    want_total = golden["episode_count"] * golden["per_episode_key_count"]
    assert rows["total_rows"] == want_total, (
        f"{label} row-count drift: got {rows['total_rows']} want {want_total} "
        f"({golden['episode_count']} episodes × {golden['per_episode_key_count']} keys/episode)"
    )


def _bindings_it_feature_present() -> bool:
    """Check whether lunaris-py was built with `bindings-it`.

    Production wheels do NOT ship the conformance helpers; Plan 08-04
    only runs this test against a `bindings-it`-built binding. When the
    helpers are absent, skip the test with a clear reason.
    """
    try:
        import lunaris as _l  # noqa: F401
        return hasattr(_l.lunaris, "conformance_fixture_episodes") and hasattr(
            _l.lunaris, "scan_kv_prefix"
        )
    except ImportError:
        return False


def test_python_driver_backend_parity():
    """Per-driver backend parity — Python-side.

    Flow:
      1. Load committed golden JSON.
      2. Probe LUNARIS_MOON_URL.
      3. If reachable: open handle, ingest FixtureCorpus, scan prefix,
         normalize, assert against golden.
      4. If unreachable, skip.
    """
    if not _bindings_it_feature_present():
        pytest.skip(
            "lunaris-py built without `bindings-it` feature — "
            "run `maturin develop --release --features bindings-it` "
            "to build the conformance helpers"
        )

    golden = _load_golden()
    moon = _probe_backend("LUNARIS_MOON_URL")

    if moon is None:
        pytest.skip(
            "LUNARIS_MOON_URL not configured with a reachable Moon — "
            "skipping per-driver parity test"
        )

    async def run() -> None:
        rows = await _ingest_and_normalize(moon, golden)
        _assert_structural_eq(rows, golden, "moon")

    asyncio.run(run())


def test_golden_reference_loads():
    """Sanity — the committed golden JSON has the expected shape.

    Offline check (no backend needed). Guards against accidental drift
    in the committed JSON: if someone changes the structure without
    updating every driver, this surfaces it.
    """
    golden = _load_golden()
    # Required keys present.
    for required in (
        "episode_count",
        "chunk_count_per_episode",
        "keys_prefix",
        "embedding_dim",
        "per_episode_key_count",
    ):
        assert required in golden, f"golden missing key: {required}"

    # Hardcoded invariants that match the Rust `FixtureCorpus`:
    assert golden["episode_count"] == COUNT
    assert golden["keys_prefix"] == "lunaris:_dev_:episode:"
    assert golden["per_episode_key_count"] == 1
