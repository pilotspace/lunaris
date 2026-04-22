"""Plan 08-02 BIND-PY-04 — Python-side toggle ergonomics.

The Rust-side codegen (`generated.rs`) exposes `GraphPipelineHandle` /
`ConsolidatorPipelineHandle` classes whose only method is the codegen-single
`toggle(on: bool)`. Python callers reach for the richer Rust surface
(`.enable()` / `.disable()` / `.is_enabled()`) via the free functions added
in `src/toggles.rs`, which this module wraps as instance methods on the
generated classes so `handle.graph_pipeline.enable()` works.

We also attach `.graph_pipeline` and `.consolidator_pipeline` PROPERTIES on
the Rust `Lunaris` class so the code-surface toggle path reads exactly like
the Rust equivalent.
"""
from __future__ import annotations

from typing import Any

from .lunaris import (  # type: ignore[attr-defined]
    GraphPipelineHandle as _RustGraphPipelineHandle,
    ConsolidatorPipelineHandle as _RustConsolidatorPipelineHandle,
    get_graph_pipeline as _get_graph_pipeline,
    get_consolidator_pipeline as _get_consolidator_pipeline,
    graph_pipeline_enable as _graph_pipeline_enable,
    graph_pipeline_disable as _graph_pipeline_disable,
    graph_pipeline_is_enabled as _graph_pipeline_is_enabled,
    consolidator_pipeline_enable as _consolidator_pipeline_enable,
    consolidator_pipeline_disable as _consolidator_pipeline_disable,
    consolidator_pipeline_is_enabled as _consolidator_pipeline_is_enabled,
)


class GraphPipelineHandle:
    """Python ergonomic wrapper around the Rust `GraphPipelineHandle`.

    Callers rarely construct this directly — they reach it via
    `handle.graph_pipeline`. Exposed as a top-level class for type-checking
    and `isinstance` convenience."""

    __slots__ = ("_inner",)

    def __init__(self, rust_handle: _RustGraphPipelineHandle) -> None:
        self._inner = rust_handle

    def enable(self) -> None:
        _graph_pipeline_enable(self._inner)

    def disable(self) -> None:
        _graph_pipeline_disable(self._inner)

    def is_enabled(self) -> bool:
        return _graph_pipeline_is_enabled(self._inner)

    # Preserve the codegen single-item surface for callers that want it.
    def toggle(self, on: bool) -> None:
        if on:
            self.enable()
        else:
            self.disable()


class ConsolidatorPipelineHandle:
    __slots__ = ("_inner",)

    def __init__(self, rust_handle: _RustConsolidatorPipelineHandle) -> None:
        self._inner = rust_handle

    def enable(self) -> None:
        _consolidator_pipeline_enable(self._inner)

    def disable(self) -> None:
        _consolidator_pipeline_disable(self._inner)

    def is_enabled(self) -> bool:
        return _consolidator_pipeline_is_enabled(self._inner)

    def toggle(self, on: bool) -> None:
        if on:
            self.enable()
        else:
            self.disable()


def attach_pipeline_properties(lunaris_cls: Any) -> None:
    """Monkey-patch `graph_pipeline` + `consolidator_pipeline` properties on
    the Rust-side `Lunaris` class so Python callers can write
    `handle.graph_pipeline.enable()` without reaching for the free
    functions. Called once from `__init__.py`."""
    # PyO3 classes accept attribute assignment at the class level when the
    # slot isn't `__slots__`-locked. The Rust side doesn't declare
    # `__slots__`, so `setattr(cls, ...)` installs a descriptor.
    def _graph_property(self: Any) -> GraphPipelineHandle:
        return GraphPipelineHandle(_get_graph_pipeline(self))

    def _consolidator_property(self: Any) -> ConsolidatorPipelineHandle:
        return ConsolidatorPipelineHandle(_get_consolidator_pipeline(self))

    try:
        setattr(lunaris_cls, "graph_pipeline", property(_graph_property))
        setattr(lunaris_cls, "consolidator_pipeline", property(_consolidator_property))
    except (AttributeError, TypeError):
        # Defensive fallback — if PyO3 ever refuses class-level assignment,
        # callers can fall back to `GraphPipelineHandle(_get_graph_pipeline(handle))`.
        pass
