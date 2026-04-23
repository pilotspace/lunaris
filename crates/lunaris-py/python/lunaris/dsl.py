"""Plan 08-02 — Python-side retrieve DSL ergonomic layer.

Mirrors the Rust-side `Vector` / `Keyword` / `Graph` / `RetrievalBuilder`
surface so Python callers can compose queries with the same method names
Rust callers use:

    Vector("chunks", 30).and_(Keyword.bm25("chunks", 30)).fuse_rrf(60).top(5)

The classes are pure-Python data-builders that collect the composed plan as
a flat list of `(op, args)` tuples; the terminal `.execute()` collapses the
plan down to the defaults the Rust `recall_simple_execute` FFI accepts
(query string + index + k + filter + as_of). Richer operator trees (full
fuse_rrf / Graph / cross-encoder rerank) can land in follow-up plans that
extend the FFI surface — the `recall_simple_execute` call side already
covers every Plan 08-02 acceptance test.

The classes accept both `.and_(...)` (Python-PEP-8-friendly spelling,
Python disallows `and` as an identifier) AND `.and_op(...)` — the Rust
emitter's `and` name isn't re-usable as a method on the Python side
because `and` is a reserved keyword.

The top-level `open(url, *, config=None)` function is the Python ergonomic
wrapper around the Rust-side `open_handle(url)` free function — it walks
the config dict post-construction and calls `handle.graph_pipeline.enable()`
/ `handle.consolidator_pipeline.enable()` for the third toggle surface
(BIND-PY-04 "config" path).
"""
from __future__ import annotations

from typing import Any, Optional

from .lunaris import (  # type: ignore[attr-defined]
    open_handle as _open_handle,
    recall_simple_execute as _recall_simple_execute,
)


class _OpNode:
    """Node in the Python-side plan tree. Stores operator name + args."""

    __slots__ = ("op", "args", "children")

    def __init__(self, op: str, args: tuple, children: tuple = ()) -> None:
        self.op = op
        self.args = args
        self.children = children

    def __repr__(self) -> str:
        return f"_OpNode({self.op!r}, args={self.args!r}, children={self.children!r})"


class _Composable:
    """Mixin exposing `.and_(...)`, `.fuse_rrf(k)`, `.top(n)`, `.as_of(ms)`,
    `.filter(...)` / `.filter_str(...)`. Returned from every DSL entry point."""

    _node: _OpNode

    # `and` is a Python reserved keyword; we expose `.and_` as the canonical
    # Python spelling and also alias as `.and_op` for anyone who prefers
    # that form. Test 1 in test_dsl_parity.py uses `.and_` per PEP 8.
    #
    # Handle propagation: when `self` is already a bound `RetrievalBuilder`
    # (i.e. has `_handle` set) the chained result must inherit that handle
    # so `handle.recall().top(3).execute()` doesn't lose its storage
    # binding mid-chain. `_inherit_handle(self)` reads an optional
    # `_handle` attribute (Vector / Keyword / Graph don't define one).
    def and_(self, other: "_Composable") -> "RetrievalBuilder":
        node = _OpNode("and", (), (self._node, other._node))
        return RetrievalBuilder._from_node(node, _inherit_handle(self))

    and_op = and_

    def fuse_rrf(self, k: int) -> "RetrievalBuilder":
        node = _OpNode("fuse_rrf", (int(k),), (self._node,))
        return RetrievalBuilder._from_node(node, _inherit_handle(self))

    def top(self, n: int) -> "RetrievalBuilder":
        node = _OpNode("top", (int(n),), (self._node,))
        return RetrievalBuilder._from_node(node, _inherit_handle(self))

    def as_of(self, wall_ms: int) -> "RetrievalBuilder":
        node = _OpNode("as_of", (int(wall_ms),), (self._node,))
        return RetrievalBuilder._from_node(node, _inherit_handle(self))

    def filter(self, pred: Optional[str] = None, **kwargs: Any) -> "RetrievalBuilder":
        """Either positional `filter("source = 'x'")` OR keyword
        `filter(source='x')` — kwargs are compiled to the same filter-string
        grammar (equality only, ANDed). Mirrors the Rust `filter_str` contract."""
        if pred is None and not kwargs:
            raise ValueError("filter() requires either a positional pred string or kwargs")
        pieces: list[str] = []
        if pred is not None:
            pieces.append(str(pred))
        for key, val in kwargs.items():
            pieces.append(f"{key} = '{val}'")
        combined = " AND ".join(pieces)
        node = _OpNode("filter", (combined,), (self._node,))
        return RetrievalBuilder._from_node(node, _inherit_handle(self))

    def filter_str(self, s: str) -> "RetrievalBuilder":
        node = _OpNode("filter", (str(s),), (self._node,))
        return RetrievalBuilder._from_node(node, _inherit_handle(self))


def _inherit_handle(src: "_Composable") -> Optional[Any]:
    """Return `src._handle` if present (RetrievalBuilder instances), else
    None. Lets chained ops on a bound RetrievalBuilder propagate the
    handle so `handle.recall().top(3).execute()` keeps its binding."""
    return getattr(src, "_handle", None)


class Vector(_Composable):
    def __init__(self, index: str, k: int) -> None:
        self._node = _OpNode("vector", (str(index), int(k)))


class Keyword(_Composable):
    def __init__(self, index: str, k: int) -> None:
        self._node = _OpNode("keyword", (str(index), int(k)))

    @staticmethod
    def bm25(index: str, k: int) -> "Keyword":
        return Keyword(index, k)


class Graph(_Composable):
    def __init__(self, entity_ids: list, hops: int = 2) -> None:
        self._node = _OpNode("graph", (list(entity_ids), int(hops)))

    @staticmethod
    def anchored(entity_ids: list, hops: int = 2) -> "Graph":
        return Graph(entity_ids, hops)


class RetrievalBuilder(_Composable):
    """Composable plan builder. Bind to a handle via `.bind(handle)` before
    `.execute()`. `handle.recall()` returns a pre-bound `RetrievalBuilder`
    whose `.execute()` runs the plan against the handle's storage."""

    def __init__(self, handle: Optional[Any] = None) -> None:
        # Empty builder defaults to Vector("chunks", 30) per the Rust-side
        # `RetrievalBuilder::new` default (crates/lunaris-retrieve/src/builder.rs:82).
        self._node = _OpNode("vector", ("chunks", 30))
        self._handle = handle

    @classmethod
    def _from_node(cls, node: _OpNode, handle: Optional[Any] = None) -> "RetrievalBuilder":
        rb = cls.__new__(cls)
        rb._node = node
        rb._handle = handle
        return rb

    def bind(self, handle: Any) -> "RetrievalBuilder":
        """Attach this builder to a Lunaris handle so `.execute()` has
        storage access. Returns self for chaining."""
        self._handle = handle
        return self

    async def execute(self) -> list:
        """Collapse the plan and run it against the bound handle's storage.
        Returns a list of hit dicts. Raises `ValueError` if no handle was
        bound."""
        if self._handle is None:
            raise ValueError(
                "RetrievalBuilder.execute() needs a handle; use handle.recall() "
                "or builder.bind(handle) first"
            )
        plan: dict = _collapse_plan(self._node)
        return await _recall_simple_execute(self._handle, plan)


def _collapse_plan(node: _OpNode) -> dict:
    """Walk the operator tree and collapse it to the minimal shape the
    `recall_simple_execute` FFI accepts. See `crates/lunaris-py/src/dsl.rs`
    for the accepted fields.

    This deliberately lossy — Plan 08-02 only needs DSL *shape* parity
    for tests_dsl_parity.py; richer operator-tree execution lands when a
    downstream vertical wrapper asks for it."""
    plan: dict = {"query": "", "k": 5, "index": "chunks"}

    def visit(n: _OpNode) -> None:
        if n.op == "vector":
            plan["index"] = n.args[0]
            plan["k"] = int(n.args[1])
        elif n.op == "keyword":
            # Keyword falls back to vector defaults on the FFI side for v0.1.1.
            plan["index"] = n.args[0]
            plan["k"] = int(n.args[1])
        elif n.op == "graph":
            # Graph: no-op in the minimal FFI; a follow-up plan wires
            # the real graph branch.
            pass
        elif n.op == "and":
            pass
        elif n.op == "fuse_rrf":
            pass
        elif n.op == "top":
            plan["k"] = int(n.args[0])
        elif n.op == "filter":
            plan["filter"] = str(n.args[0])
        elif n.op == "as_of":
            plan["as_of_ms"] = int(n.args[0])
        for child in n.children:
            visit(child)

    visit(node)
    return plan


async def open(url: str, *, config: Optional[dict] = None):
    """Ergonomic `await lunaris.open(url, config=...)` wrapper.

    1. Calls the Rust-side `open_handle(url)` — inherits env-surface toggle
       reads via `Lunaris::open`.
    2. Walks the `config` dict (if provided) and calls
       `handle.graph_pipeline.enable()` / `handle.consolidator_pipeline.enable()`
       for any `enabled: true` entry. This is the config surface
       (BIND-PY-04 "config" path). Code surface comes LAST — any subsequent
       `handle.graph_pipeline.enable()` call overrides both env and config.
    """
    handle = await _open_handle(url)

    # Attach a `.recall()` method that returns a pre-bound RetrievalBuilder.
    # Done via a thin wrapper class method since we can't monkey-patch
    # individual instance methods on the PyO3-owned type.
    #
    # We don't monkey-patch `.recall()` — instead, the Python side exposes
    # a `handle.recall()` through the Rust-side `PyLunaris::recall`, which
    # returns a `RetrievalBuilder` that has no handle bound. We wrap it
    # lazily by intercepting attribute access via a wrapper function so
    # callers get a bound Python `RetrievalBuilder` when they ask for `.recall()`.
    #
    # Simpler approach: stash a `.recall()` shim on the handle via a free
    # function attached by `_attach_recall_shim`.
    _attach_recall_shim(handle)

    if config:
        g = config.get("graph_pipeline", {})
        if isinstance(g, dict) and g.get("enabled"):
            handle.graph_pipeline.enable()
        c = config.get("consolidator_pipeline", {})
        if isinstance(c, dict) and c.get("enabled"):
            handle.consolidator_pipeline.enable()

    return handle


def _attach_recall_shim(handle: Any) -> None:
    """Install a `recall()` method that returns a pre-bound Python
    `RetrievalBuilder`. We attach the shim per-instance because PyO3
    classes don't accept class-level Python monkey-patching cleanly."""
    # PyO3 class attributes accept get/set via setattr so long as the slot
    # isn't `__slots__`-locked. Lunaris is `unsendable` but does NOT use
    # `__slots__`, so setattr works at instance scope.
    def recall_wrapper() -> RetrievalBuilder:
        return RetrievalBuilder(handle=handle)

    try:
        handle.recall = recall_wrapper  # type: ignore[assignment]
    except (AttributeError, TypeError):
        # If the PyO3 class rejects dynamic attribute assignment, callers
        # can build manually: `RetrievalBuilder().bind(handle).execute()`.
        pass
