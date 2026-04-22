"""Plan 08-02 BIND-PY-03 — Python DSL parity with the Rust surface.

Offline tests: every class/method is purely Python-side structure building;
no backend is required. Test 3 (`test_filter_str`) does reach for a handle
so it SKIPs when no Moon backend is available.
"""
from __future__ import annotations

import pytest

import lunaris
from lunaris import Vector, Keyword, Graph, RetrievalBuilder


def test_vector_and_keyword_fuse_top_shape() -> None:
    """Rust: `Vector::new(...).and(Keyword::bm25(...)).fuse_rrf(60).top(5)`."""
    plan = (
        Vector("chunks", 30)
        .and_(Keyword.bm25("chunks", 30))
        .fuse_rrf(60)
        .top(5)
    )
    assert isinstance(plan, RetrievalBuilder)


def test_graph_anchored_and_as_of_shape() -> None:
    """Rust: `Graph::anchored(...).and(Vector(...)).as_of(t).top(5)`."""
    plan = (
        Graph.anchored(["alice", "bob"], hops=2)
        .and_(Vector("chunks", 10))
        .as_of(1_000_000)
        .top(5)
    )
    assert isinstance(plan, RetrievalBuilder)


def test_filter_kwargs_and_positional_shape() -> None:
    """Both `filter(source='x')` and `filter_str('source = "x"')` work."""
    a = Vector("chunks", 5).filter(source="helios:fs/42").top(3)
    b = Vector("chunks", 5).filter_str("source = 'helios:fs/42'").top(3)
    assert isinstance(a, RetrievalBuilder)
    assert isinstance(b, RetrievalBuilder)


def test_method_names_match_rust_surface() -> None:
    """BIND-PY-03 — public method names on the Python DSL classes are
    the exact names used in the Rust retrieve DSL."""
    # Vector / Keyword / Graph each expose `and_`, `fuse_rrf`, `top`,
    # `as_of`, `filter`, `filter_str`. Keyword has the `bm25` factory.
    for cls in (Vector, Keyword, Graph, RetrievalBuilder):
        for meth in ("and_", "fuse_rrf", "top", "as_of", "filter", "filter_str"):
            assert hasattr(cls, meth), f"{cls.__name__} missing .{meth}"
    assert hasattr(Keyword, "bm25")
    assert hasattr(Graph, "anchored")


@pytest.mark.asyncio
async def test_execute_returns_list(moon_backend_url: str) -> None:
    """Test 2 (from the plan) — `RetrievalBuilder.execute()` returns a list
    even against an empty backend."""
    handle = await lunaris.open(moon_backend_url)
    hits = await handle.recall().top(3).execute()
    assert isinstance(hits, list)


@pytest.mark.asyncio
async def test_filter_str_runs(moon_backend_url: str) -> None:
    handle = await lunaris.open(moon_backend_url)
    hits = await handle.recall().filter(source="nonexistent").top(3).execute()
    assert isinstance(hits, list)
    hits2 = await handle.recall().filter_str("source = 'nonexistent'").top(3).execute()
    assert isinstance(hits2, list)
