"""Lunaris — agent memory engine Python bindings.

Top-level module. Re-exports the compiled cdylib (`lunaris.lunaris`) classes
and adds a thin Python-side ergonomic layer (`open` wrapper that walks the
config-dict toggle surface; `.graph_pipeline` / `.consolidator_pipeline`
property accessors on the `Lunaris` handle).

See https://github.com/lunaris-dev/lunaris + Phase 8 Plan 08-02.
"""
from __future__ import annotations

# Compiled PyO3 cdylib.
from .lunaris import (  # type: ignore[attr-defined]
    Lunaris as _RustLunaris,
    Vector as _RustVector,
    Keyword as _RustKeyword,
    Graph as _RustGraph,
    RetrievalBuilder as _RustRetrievalBuilder,
    GraphPipelineHandle as _RustGraphPipelineHandle,
    ConsolidatorPipelineHandle as _RustConsolidatorPipelineHandle,
    LunarisError,
    __version__,
    # Free functions injected by the handwritten src/dsl.rs + src/toggles.rs.
    open_handle as _open_handle,
    recall_simple_execute as _recall_simple_execute,
    get_graph_pipeline as _get_graph_pipeline,
    get_consolidator_pipeline as _get_consolidator_pipeline,
    graph_pipeline_enable as _graph_pipeline_enable,
    graph_pipeline_disable as _graph_pipeline_disable,
    graph_pipeline_is_enabled as _graph_pipeline_is_enabled,
    consolidator_pipeline_enable as _consolidator_pipeline_enable,
    consolidator_pipeline_disable as _consolidator_pipeline_disable,
    consolidator_pipeline_is_enabled as _consolidator_pipeline_is_enabled,
    from_env,
    from_config,
)
from .dsl import Vector, Keyword, Graph, RetrievalBuilder, open
from .toggles import (
    GraphPipelineHandle,
    ConsolidatorPipelineHandle,
    attach_pipeline_properties,
)

# Patch the Rust-side `Lunaris` class with Python-side property accessors so
# callers can write `handle.graph_pipeline.enable()` instead of reaching for
# the raw free-function form.
attach_pipeline_properties(_RustLunaris)

Lunaris = _RustLunaris  # Public re-export under the ergonomic name.

__all__ = [
    "open",
    "Lunaris",
    "Vector",
    "Keyword",
    "Graph",
    "RetrievalBuilder",
    "GraphPipelineHandle",
    "ConsolidatorPipelineHandle",
    "LunarisError",
    "from_env",
    "from_config",
    "__version__",
]
