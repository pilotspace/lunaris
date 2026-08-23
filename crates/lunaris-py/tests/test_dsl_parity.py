"""Plan 08-02 BIND-PY-03 — Python DSL parity with the Rust surface.

Offline tests: every class/method is purely Python-side structure building;
no backend is required. Test 3 (`test_filter_str`) does reach for a handle
so it SKIPs when no Moon backend is available.
"""
from __future__ import annotations

import pytest

import lunaris
from lunaris import Vector, Keyword, Graph, RetrievalBuilder
from lunaris.dsl import _collapse_plan


def marshalled(plan: RetrievalBuilder) -> dict:
    """The plan dict `.execute()` would send across the FFI.

    Every test below asserts on THIS rather than on `isinstance(plan,
    RetrievalBuilder)`. The isinstance form was vacuous: the builder classes
    return a builder from every combinator, so it held whenever a constructor
    ran at all — it passed identically while the SDK refused to execute the
    very plan it was checking.
    """
    return _collapse_plan(plan._node)


def test_vector_and_keyword_fuse_top_marshals_both_legs() -> None:
    """Rust: `Vector::new(...).and(Keyword::bm25(...)).fuse_rrf(60).top(5)`."""
    plan = (
        Vector("chunks", 30)
        .and_(Keyword.bm25("chunks", 30))
        .fuse_rrf(60)
        .top(5)
    )
    assert marshalled(plan)["root"] == {
        "op": "top",
        "n": 5,
        "child": {
            "op": "fuse_rrf",
            "k": 60,
            "child": {
                "op": "and",
                "left": {"op": "vector", "index": "chunks", "k": 30},
                "right": {"op": "keyword", "index": "chunks", "k": 30},
            },
        },
    }


def test_flipping_the_operands_marshals_a_different_plan() -> None:
    """Pre-F14 the collapse let the LAST leg visited win `index` and `k`, so
    both orders produced the same single-leg plan and which index got searched
    depended on the order the operands were written in."""
    a = marshalled(Vector("chunks", 10).and_(Keyword.bm25("facts", 20)))["root"]
    b = marshalled(Keyword.bm25("facts", 20).and_(Vector("chunks", 10)))["root"]
    assert a["left"] == {"op": "vector", "index": "chunks", "k": 10}
    assert b["left"] == {"op": "keyword", "index": "facts", "k": 20}
    assert a != b


def test_graph_anchored_and_as_of_marshals_seeds_hops_and_the_witness() -> None:
    """Rust: `Graph::anchored(...).and(Vector(...)).as_of(t).top(5)`."""
    plan = (
        Graph.anchored([("Alice", "Person"), ("Bob", "Person")], hops=2)
        .and_(Vector("chunks", 10))
        .as_of(1_000_000)
        .top(5)
    )
    got = marshalled(plan)
    assert got["as_of_ms"] == 1_000_000
    graph_leg = got["root"]["child"]["left"]
    assert graph_leg == {
        "op": "graph",
        "seeds": [
            {"name": "Alice", "type": "Person"},
            {"name": "Bob", "type": "Person"},
        ],
        "hops": 2,
    }


def test_a_bare_name_is_not_an_anchor() -> None:
    """An EntityId is derived from (name, type), so a bare name would need a
    guessed type. The guess yields a well-formed id that matches nothing, and
    a traversal from nothing returns empty — indistinguishable from a real
    absence of edges. So it raises instead."""
    with pytest.raises(NotImplementedError) as e:
        marshalled(Graph.anchored(["alice"], hops=2))
    assert "name" in str(e.value) and "type" in str(e.value)


def test_two_different_queries_in_one_plan_raise() -> None:
    """A plan runs ONE query against every leg. Two would mean one of them is
    dropped, and the caller would never learn which."""
    left = Vector("chunks", 10).query("what does Alice like?")
    right = Keyword.bm25("facts", 10).query("where does Bob live?")
    with pytest.raises(NotImplementedError):
        marshalled(left.and_(right))


def test_filter_kwargs_and_positional_reach_the_envelope() -> None:
    """Both `filter(source='x')` and `filter_str('source = "x"')` work, and
    both land in the plan rather than being dropped on the way."""
    a = marshalled(Vector("chunks", 5).filter(source="helios:fs/42").top(3))
    b = marshalled(Vector("chunks", 5).filter_str("source = 'helios:fs/42'").top(3))
    assert "helios:fs/42" in a["filter"]
    assert "helios:fs/42" in b["filter"]
    assert a["root"]["op"] == "top" and a["root"]["n"] == 3


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


@pytest.mark.asyncio
async def test_a_composed_plan_actually_executes(moon_backend_url: str) -> None:
    """F14 — the composed plan the parity tests build must RUN, not just
    marshal. Before F14 this raised `NotImplementedError` at `.execute()`."""
    handle = await lunaris.open(moon_backend_url)
    hits = await (
        handle.recall()
        .query("chocolate")
        .top(3)
        .execute()
    )
    assert isinstance(hits, list)

    composed = (
        Vector("chunks", 30)
        .and_(Keyword.bm25("chunks", 30))
        .fuse_rrf(60)
        .top(5)
        .query("chocolate")
        .bind(handle)
    )
    fused = await composed.execute()
    assert isinstance(fused, list), "a two-leg fused plan must execute, not raise"
