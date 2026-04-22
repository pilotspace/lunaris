"""Plan 08-02 BIND-PY-04 — three-surface pipeline toggle tests.

Three independent tests, one per surface:

- **Code**: `handle.graph_pipeline.enable()` flips the bit in-process.
- **Env**:  `LUNARIS_GRAPH_ENABLED=1` at open-time sets the initial state.
- **Config**: `lunaris.open(url, config={"graph_pipeline": {"enabled": True}})`.

The first two require a live backend (they construct a real handle and
query `is_enabled()`). The third tests the pure `from_config` resolver
reader — shape-only, offline.
"""
from __future__ import annotations

import os

import pytest

import lunaris


@pytest.mark.asyncio
async def test_code_surface_enables_graph_pipeline(moon_backend_url: str) -> None:
    handle = await lunaris.open(moon_backend_url)
    assert handle.graph_pipeline.is_enabled() is False
    handle.graph_pipeline.enable()
    assert handle.graph_pipeline.is_enabled() is True
    handle.graph_pipeline.disable()
    assert handle.graph_pipeline.is_enabled() is False


def test_env_surface_pure_reader(monkeypatch: pytest.MonkeyPatch) -> None:
    """`lunaris.from_env()` reads LUNARIS_GRAPH_ENABLED / LUNARIS_CONSOLIDATE_ENABLED
    directly — exercises the Rust-side `GraphPipelineHandle::initial_state_from_env`
    path without requiring a handle / backend."""
    monkeypatch.setenv("LUNARIS_GRAPH_ENABLED", "1")
    monkeypatch.setenv("LUNARIS_CONSOLIDATE_ENABLED", "0")
    g, c = lunaris.from_env()
    assert g is True
    assert c is False

    monkeypatch.delenv("LUNARIS_GRAPH_ENABLED", raising=False)
    monkeypatch.delenv("LUNARIS_CONSOLIDATE_ENABLED", raising=False)
    g2, c2 = lunaris.from_env()
    assert g2 is False
    assert c2 is False


def test_config_surface_pure_reader() -> None:
    """`lunaris.from_config(dict)` applies the same `Some("1"|"true"|...)`
    semantics as the env reader — offline, no handle needed."""
    cfg = {
        "graph_pipeline": {"enabled": True},
        "consolidator_pipeline": {"enabled": False},
    }
    g, c = lunaris.from_config(cfg)
    assert g is True
    assert c is False

    # Missing keys default to False (blueprint §5.1 / §5.2 default OFF).
    g2, c2 = lunaris.from_config({})
    assert g2 is False
    assert c2 is False


@pytest.mark.asyncio
async def test_config_surface_enables_via_open(moon_backend_url: str) -> None:
    handle = await lunaris.open(
        moon_backend_url,
        config={"graph_pipeline": {"enabled": True}},
    )
    assert handle.graph_pipeline.is_enabled() is True

    handle2 = await lunaris.open(
        moon_backend_url,
        config={"consolidator_pipeline": {"enabled": True}},
    )
    assert handle2.consolidator_pipeline.is_enabled() is True
