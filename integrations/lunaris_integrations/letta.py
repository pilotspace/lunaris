"""Letta archival-memory connector — back Letta archival memory with Lunaris.

**Degrade path (per the frozen-contract freeze flag).** Letta's archival store
(`letta.services.passage_manager`) is server-side and DB-coupled (asyncpg) —
there is no clean lightweight connector base to subclass at the pinned version.
So this ships as a thin client-backed SHIM that maps Letta's archival
insert/search onto the shared `LunarisClient`, PLUS a recipe doc
(`examples/letta-lunaris/README.md`) showing how to point a Letta deployment at
lunaris-server. The shim's mapping is fully tested; the live Letta-server wiring
is the recipe.

- `insert(passage)`         → `client.ingest`
- `search(query, top_k)`    → `client.recall(k=top_k)`

    pip install lunaris-integrations[letta]

The Letta archival schema presence is version-guarded; drift raises
`UnsupportedFrameworkVersion`.
"""
from __future__ import annotations

import asyncio
from typing import Any

from .client import LunarisClient, UnsupportedFrameworkVersion

# Version-guard: the shim speaks Letta's archival schema vocabulary. If those
# schemas move, fail loud instead of mis-mapping.
try:
    from letta.schemas.passage import Passage, PassageCreate
except ImportError as exc:  # pragma: no cover - drift path
    raise UnsupportedFrameworkVersion(
        "letta", "unknown", ">=0.16 with letta.schemas.passage"
    ) from exc


def _run(coro: Any) -> Any:
    return asyncio.run(coro)


def _passage_text(passage: Any) -> str:
    """Extract the text from a Letta `Passage`/`PassageCreate` or a raw string."""
    if isinstance(passage, (Passage, PassageCreate)):
        return passage.text
    if hasattr(passage, "text"):
        return str(passage.text)
    return str(passage)


class LunarisArchivalConnector:
    """Thin Letta archival shim over a (scope-bound) `LunarisClient`."""

    def __init__(self, client: LunarisClient, *, agent_id: str | None = None) -> None:
        self._client = client
        self._agent_id = agent_id

    def insert(self, passage: Any) -> str:
        """Insert one archival passage → `client.ingest`. Returns the LSN."""
        return _run(
            self._client.ingest(
                source="letta",
                content=_passage_text(passage),
                metadata={"agent_id": self._agent_id} if self._agent_id else None,
            )
        )

    def search(self, query: str, top_k: int = 10) -> list[dict[str, Any]]:
        """Search archival memory → `client.recall(k=top_k)` → result dicts."""
        hits = _run(self._client.recall(query, k=top_k))
        return [
            {"id": h.id, "content": h.content, "score": h.score, "source": h.source}
            for h in hits
        ]
