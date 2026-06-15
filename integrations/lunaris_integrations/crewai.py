"""CrewAI storage adapter — back CrewAI memory with Lunaris.

Maps the CrewAI RAG storage interface onto the shared `LunarisClient`:

- `save(value, metadata)`        → `client.ingest`
- `search(query, limit, ...)`    → `client.recall(k=limit)`  → list[dict]
- `reset()`                      → `client.forget_scope`

    pip install lunaris-integrations[crewai]

We subclass CrewAI's RAG storage base (so `isinstance` holds and CrewAI accepts
it) but use a light client-backed `__init__` that does NOT trigger CrewAI's
embedder/agent initialisation — Lunaris owns embedding + recall. The base shape
is version-guarded; drift raises `UnsupportedFrameworkVersion`.
"""
from __future__ import annotations

import asyncio
from typing import Any

from .client import LunarisClient, UnsupportedFrameworkVersion, require_base_methods

# CrewAI moved its storage base across versions; support the current
# (`crewai.rag.storage.base_rag_storage.BaseRAGStorage`) and the older
# (`crewai.memory.storage.interface.Storage`) layouts. Anything else → drift.
try:
    from crewai.rag.storage.base_rag_storage import BaseRAGStorage as _CrewBase
except ImportError:  # pragma: no cover - exercised only on older CrewAI
    try:
        from crewai.memory.storage.interface import Storage as _CrewBase
    except ImportError as exc:  # pragma: no cover - drift path
        raise UnsupportedFrameworkVersion(
            "crewai", "unknown", ">=0.x with a RAG storage base (save/search/reset)"
        ) from exc

require_base_methods(
    _CrewBase,
    {"save", "search", "reset"},
    framework="crewai",
    found=getattr(_CrewBase, "__module__", "unknown"),
    expected="a storage base declaring save/search/reset",
)


def _run(coro: Any) -> Any:
    return asyncio.run(coro)


class LunarisCrewAIStorage(_CrewBase):  # type: ignore[misc, valid-type]
    """CrewAI RAG storage backed by a (already scope-bound) `LunarisClient`."""

    def __init__(self, client: LunarisClient, *, type: str = "lunaris") -> None:
        # Light init — set the attrs CrewAI may read WITHOUT calling the heavy
        # embedder/agent bootstrap (`super().__init__` → `_initialize_agents`).
        # Lunaris owns embedding + recall, so no CrewAI embedder is needed.
        self._client = client
        self.type = type
        self.allow_reset = True
        self.embedder_config = None
        self.crew = None
        self.agents = []

    def save(self, value: Any, metadata: dict[str, Any] | None = None) -> None:
        _run(
            self._client.ingest(
                source="crewai", content=str(value), metadata=metadata
            )
        )

    def search(
        self,
        query: str,
        limit: int = 3,
        filter: dict[str, Any] | None = None,
        score_threshold: float = 0.0,
    ) -> list[dict[str, Any]]:
        hits = _run(self._client.recall(query, k=limit))
        return [
            {
                "id": h.id,
                "context": h.content,
                "metadata": {"source": h.source},
                "score": h.score,
            }
            for h in hits
            if h.score >= score_threshold
        ]

    def reset(self) -> None:
        _run(self._client.forget_scope())

    def _sanitize_role(self, role_name: str) -> str:
        # CrewAI uses this to build collection names; a Lunaris scope is bound
        # at the client, so just normalise the role string defensively.
        return role_name.replace("\n", " ").replace("/", "_").strip()
