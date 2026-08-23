"""Plan 08-02 — Python-side retrieve DSL ergonomic layer.

Mirrors the Rust-side `Vector` / `Keyword` / `Graph` / `RetrievalBuilder`
surface so Python callers can compose queries with the same method names
Rust callers use:

    Vector("chunks", 30).and_(Keyword.bm25("chunks", 30)).fuse_rrf(60).top(5)

The query text itself is set with `.query(text)`, the Python spelling of the
Rust terminal `builder.execute(Query::text(t))`:

    handle.recall().query("what does Alice like?").top(5).execute()

The classes are pure-Python data-builders that collect the composed plan as
an operator tree; the terminal `.execute()` marshals that tree into the plan
dict the Rust `recall_simple_execute` FFI executes, where
`lunaris_retrieve::plan::retriever_from_json` — the single parser both the
Python and TypeScript SDKs marshal into — turns it back into a retriever.
Composition therefore runs as written: `.and_()`, `.fuse_rrf()`, `.top()` and
a graph leg all reach the engine (F14). `query` / `as_of` / `filter` stay
envelope-level because they are builder state in Rust too.

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

import re
from typing import Any, Optional

from .lunaris import (  # type: ignore[attr-defined]
    open_handle as _open_handle,
    recall_simple_execute as _recall_simple_execute,
    # Phase 21 Plan 21-01 — kwarg passthroughs for `open(url, embedder=...,
    # reranker=...)`. Imported here so the module-local `open()` wrapper can
    # apply them post-construction without exposing the helper names at the
    # `lunaris.*` package surface.
    lunaris_with_embedder as _lunaris_with_embedder,
    lunaris_with_reranker as _lunaris_with_reranker,
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
    """Mixin exposing `.and_(...)`, `.fuse_rrf(k)`, `.top(n)`, `.query(text)`,
    `.as_of(ms)`, `.filter(...)` / `.filter_str(...)`. Returned from every DSL
    entry point."""

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

    def query(self, text: str) -> "RetrievalBuilder":
        """Set the query text the plan searches for.

        Mirrors the Rust terminal `builder.execute(Query::text(t))`. Without
        it the plan searches for the empty string, which is what every
        `.execute()` did before v0.7.1 — the DSL had no way to express a
        text query at all.

            handle.recall().query("what does Alice like?").top(5).execute()
        """
        node = _OpNode("query", (str(text),), (self._node,))
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
    """Marshal the operator tree into the plan dict the FFI executes.

    The name is historical. It no longer collapses: F14 replaced the flat
    ``{index, k}`` FFI with a ``{"root": <operator tree>}`` one, so the tree a
    caller composes is the tree the engine builds. ``lunaris_retrieve::plan``
    (Rust) is the single parser both SDKs marshal into.

    Three things stay envelope-level rather than becoming tree nodes —
    ``query``, ``as_of`` and ``filter`` — because they are builder state on
    the Rust side too, not retrievers: one query text, one as-of witness and
    one filter narrow the whole plan. That is why setting any of them twice
    with different values raises: the tree has branches, the envelope does
    not, and picking one of the two silently would run a query the caller did
    not write.

    Anything this function cannot marshal raises instead of being dropped. A
    caller who gets an exception goes and reads this docstring; a caller who
    gets a plausible list of hits does not.
    """
    envelope: dict = {"query": ""}

    def set_envelope(key: str, value: object, op: str) -> None:
        if key in envelope and envelope[key] != value and key != "query":
            raise NotImplementedError(
                f"this plan sets .{op}() twice with different values "
                f"({envelope[key]!r} and {value!r}), but the plan carries ONE "
                f"{key}: it narrows every branch at once. Split the plan, or "
                f"apply .{op}() once at the top."
            )
        if key == "query" and envelope["query"] not in ("", value):
            raise NotImplementedError(
                f"this plan sets .query() twice with different text "
                f"({envelope['query']!r} and {value!r}), but a plan runs ONE "
                f"query against every leg. Apply .query() once at the top."
            )
        envelope[key] = value

    def only_child(n: _OpNode) -> _OpNode:
        if len(n.children) != 1:
            raise NotImplementedError(
                f"`{n.op}` wraps exactly one plan, got {len(n.children)}"
            )
        return n.children[0]

    def build(n: _OpNode) -> dict:
        if n.op == "query":
            set_envelope("query", str(n.args[0]), "query")
            return build(only_child(n))
        if n.op == "as_of":
            set_envelope("as_of_ms", int(n.args[0]), "as_of")
            return build(only_child(n))
        if n.op == "filter":
            set_envelope("filter", str(n.args[0]), "filter")
            return build(only_child(n))
        if n.op in ("vector", "keyword"):
            return {"op": n.op, "index": str(n.args[0]), "k": int(n.args[1])}
        if n.op == "graph":
            return {
                "op": "graph",
                "seeds": [_marshal_seed(s, i) for i, s in enumerate(n.args[0])],
                "hops": int(n.args[1]),
            }
        if n.op == "and":
            if len(n.children) != 2:
                raise NotImplementedError(
                    f"`and` joins exactly two plans, got {len(n.children)}"
                )
            return {
                "op": "and",
                "left": build(n.children[0]),
                "right": build(n.children[1]),
            }
        if n.op == "fuse_rrf":
            return {"op": "fuse_rrf", "k": int(n.args[0]), "child": build(only_child(n))}
        if n.op == "top":
            return {"op": "top", "n": int(n.args[0]), "child": build(only_child(n))}
        raise NotImplementedError(
            f"the Python SDK has no marshalling for operator `{n.op}`, so this "
            f"plan cannot be sent to the engine as written. Drive the full "
            f"operator tree from the Rust API, or add an arm here and the "
            f"matching one in `lunaris_retrieve::plan`."
        )

    envelope["root"] = build(node)
    return envelope


_HEX32 = re.compile(r"[0-9a-fA-F]{32}")


def _marshal_seed(seed: object, index: int) -> object:
    """Render one graph anchor into the seed shape the engine accepts.

    A seed is an entity IDENTITY, and the engine derives that identity from
    ``(name, type)`` — the same "Alice" is a different anchor as a Person than
    as a Place. So a bare ``"alice"`` is rejected rather than paired with a
    guessed type: guessing yields a valid-looking EntityId that matches
    nothing, and a traversal anchored on nothing returns an empty result that
    is indistinguishable from "no such relationship exists".
    """
    if isinstance(seed, dict):
        if "name" in seed and "type" in seed:
            out = {"name": str(seed["name"]), "type": str(seed["type"])}
            if "confidence" in seed:
                out["confidence"] = float(seed["confidence"])
            return out
        raise NotImplementedError(
            f"graph seed {index} is a dict without both 'name' and 'type': {seed!r}"
        )
    if isinstance(seed, (tuple, list)) and len(seed) == 2:
        return {"name": str(seed[0]), "type": str(seed[1])}
    # `int(seed, 16)` is NOT the check to use here: Python accepts underscores
    # and surrounding whitespace, so `"…abc_ef"` would pass on this side and
    # fail the TypeScript twin's `/^[0-9a-f]{32}$/i`. Two SDKs that accept
    # different inputs for the same documented shape is the parity bug the
    # marshallers exist to avoid.
    if isinstance(seed, str) and _HEX32.fullmatch(seed):
        return seed
    raise NotImplementedError(
        f"graph seed {index} ({seed!r}) is neither a 32-char hex EntityId nor a "
        f"(name, type) pair. Pass Graph.anchored([{{'name': 'Alice', "
        f"'type': 'Person'}}], hops=2), a ('Alice', 'Person') tuple, or the hex "
        f"id the engine emitted. A bare name would need a guessed entity type, "
        f"and the wrong type anchors the traversal on an entity that does not "
        f"exist \u2014 which returns empty, exactly like a real absence of edges."
    )


async def open(
    url: str,
    *,
    config: Optional[dict] = None,
    embedder: Optional[Any] = None,
    reranker: Optional[Any] = None,
):
    """Ergonomic `await lunaris.open(url, config=..., embedder=..., reranker=...)` wrapper.

    1. Calls the Rust-side `open_handle(url)` — inherits env-surface toggle
       reads via `Lunaris::open`.
    2. **Phase 21 Plan 21-01** — if `embedder` is an `EmbedderConfig` instance,
       swaps the handle's embedder via `_lunaris_with_embedder`. Same for
       `reranker` / `RerankerConfig`. Both `None` preserves the env-driven
       default (existing callers see no behaviour change).
    3. Walks the `config` dict (if provided) and calls
       `handle.graph_pipeline.enable()` / `handle.consolidator_pipeline.enable()`
       for any `enabled: true` entry. This is the config surface
       (BIND-PY-04 "config" path). Code surface comes LAST — any subsequent
       `handle.graph_pipeline.enable()` call overrides both env and config.
    """
    handle = await _open_handle(url)

    # Phase 21 — apply the embedder + reranker overrides BEFORE the config-dict
    # toggles so the toggle handlers see the final embedder choice (matters for
    # consolidator pipelines that re-embed on flush).
    if embedder is not None:
        handle = _lunaris_with_embedder(handle, embedder)
    if reranker is not None:
        handle = _lunaris_with_reranker(handle, reranker)

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
