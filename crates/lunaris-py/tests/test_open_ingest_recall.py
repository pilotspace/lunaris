"""Plan 08-02 Task 1 — open / ingest / recall / forget / snapshot roundtrip.

These tests exercise the full 5-method Rust surface behind the Lunaris
handle. They REQUIRE a reachable Moon backend (`LUNARIS_MOON_URL` env
var; default `moon://127.0.0.1:6380`) and `pytest.skip` when none is
available — so the suite still passes on a fresh clone without a dev box.

Offline behaviours (URL-parse error mapping to `lunaris.LunarisError`) are
covered unconditionally.
"""
from __future__ import annotations

import asyncio

import pytest

import lunaris


def _build_episode(content: str) -> dict:
    """Construct a dict shaped like `lunaris_core::primitives::Episode`.

    Mirrors the `serde` shape from `crates/lunaris-core/src/primitives.rs`.
    All optional fields are filled with zero-valued defaults.

    v0.2 (RFC 0001): the `scope` field is now required. We pass `"_dev_"` here
    so the existing backwards-compat tests continue to pass. Production callers
    should use `engine.scoped(Scope(...)).ingest(EpisodeBuilder(...))` instead
    of constructing the raw Episode dict.
    """
    import ulid

    ep_id = ulid.ULID()  # type: ignore[call-arg]
    return {
        "id": str(ep_id),
        "scope": "_dev_",
        "source": "py-test",
        "content": content,
        "t_ref": None,
        "bt": {
            "valid": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
            "sys":   [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
        },
        "metadata": {},
    }


@pytest.mark.asyncio
async def test_errors_map_to_lunaris_error() -> None:
    """Unsupported URL scheme must raise `lunaris.LunarisError`. Runs offline."""
    with pytest.raises(lunaris.LunarisError) as exc_info:
        await lunaris.open("unsupported-scheme://nowhere")
    # LunarisError args are (code, message) per src/errors.rs.
    args = exc_info.value.args
    assert len(args) == 2
    code, message = args
    assert code == "STORAGE"
    assert isinstance(message, str)
    # The inner message format is "storage: {variant_display}" — variant
    # display for `UnsupportedScheme(String)` is "unsupported scheme: {s}".
    assert "scheme" in message.lower()


@pytest.mark.asyncio
async def test_open_roundtrip(moon_backend_url: str) -> None:
    handle = await lunaris.open(moon_backend_url)
    assert handle is not None
    # Integer version sanity — inherits from Rust env!("CARGO_PKG_VERSION").
    assert isinstance(lunaris.__version__, str)


@pytest.mark.asyncio
async def test_ingest_recall_roundtrip(moon_backend_url: str) -> None:
    try:
        import ulid  # noqa: F401
    except ImportError:
        pytest.skip("python-ulid not installed")

    handle = await lunaris.open(moon_backend_url)
    ep = _build_episode("the quick brown fox jumps over the lazy dog")
    lsn = await handle.ingest(ep)
    assert isinstance(lsn, str)
    assert ":" in lsn  # Lsn Display format is "{wall_ms}:{counter}".

    hits = await handle.recall().execute()
    assert isinstance(hits, list)


@pytest.mark.asyncio
async def test_snapshot_roundtrip(moon_backend_url: str) -> None:
    handle = await lunaris.open(moon_backend_url)
    s1 = await handle.snapshot()
    s2 = await handle.snapshot()
    assert isinstance(s1, str) and isinstance(s2, str)
    # Strict monotonicity per Plan 08-00's atomic_write contract — s2 >= s1.
    # Compare on the numeric parts rather than string-compare because
    # ordering isn't lexicographic after certain wall/counter transitions.
    def parse(s: str) -> tuple[int, int]:
        a, b = s.split(":", 1)
        return int(a), int(b)
    assert parse(s2) >= parse(s1)


@pytest.mark.asyncio
async def test_forget_dry_run_roundtrip(moon_backend_url: str) -> None:
    try:
        import ulid
    except ImportError:
        pytest.skip("python-ulid not installed")

    handle = await lunaris.open(moon_backend_url)
    # Build the minimum-shape ForgetRequest dict — source-prefix scope with
    # dry-run option so we never actually mutate the backend.
    req = {
        "target": {"Scope": {"BySource": "py-test/nonexistent-scope"}},
        "options": {"hard": False, "dry_run": True, "confirmation_token": None},
    }
    receipt = await handle.forget(req)
    assert isinstance(receipt, dict)
    assert receipt.get("preview") is True
    # rows_written + rows_deleted both 0 for a dry-run.
    assert receipt.get("rows_written") == 0
    assert receipt.get("rows_deleted") == 0
