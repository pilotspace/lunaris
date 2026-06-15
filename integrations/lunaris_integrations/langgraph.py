"""LangGraph `BaseStore` adapter — persist agent memory to Lunaris.

Maps the LangGraph store ops onto the shared `LunarisClient`:

- `aput(namespace, key, value)` → `client.ingest`  (scoped by the namespace)
- `aget(namespace, key)`        → `client.recall`   (key-keyed, best-effort)
- `asearch(namespace, query)`   → `client.recall`   (semantic top-k)

The class is transport-agnostic: it holds a `client_factory` and never branches
on whether the underlying client is HTTP or in-process SDK.

    pip install lunaris-integrations[langgraph]

The module imports `langgraph` at load time and version-guards the BaseStore
shape (raises `UnsupportedFrameworkVersion` on drift — never a silent mis-map).
"""
from __future__ import annotations

import asyncio
from datetime import datetime, timezone
from importlib import metadata
from typing import Any, Callable

from langgraph.store.base import (
    BaseStore,
    GetOp,
    Item,
    PutOp,
    SearchItem,
    SearchOp,
)

from .client import (
    Hit,
    LunarisClient,
    namespace_to_scope,
    require_base_methods,
)

# Version-guard the pinned BaseStore shape. If a future LangGraph drops one of
# these the adapter fails loud at import instead of mis-mapping at runtime.
try:
    _LANGGRAPH_VERSION = metadata.version("langgraph")
except metadata.PackageNotFoundError:  # pragma: no cover - defensive
    _LANGGRAPH_VERSION = "unknown"

require_base_methods(
    BaseStore,
    {"aput", "aget", "asearch", "abatch", "batch"},
    framework="langgraph",
    found=_LANGGRAPH_VERSION,
    expected=">=0.2 (BaseStore with aput/aget/asearch/abatch)",
)


def _extract_text(value: dict[str, Any]) -> str:
    """Pull the natural-language content out of a LangGraph value dict so it is
    semantically searchable; fall back to a stable JSON encoding."""
    for field in ("text", "content", "data"):
        v = value.get(field)
        if isinstance(v, str) and v:
            return v
    import json

    return json.dumps(value, sort_keys=True)


def _now() -> datetime:
    return datetime.now(timezone.utc)


class LunarisStore(BaseStore):
    """A LangGraph `BaseStore` backed by Lunaris memory."""

    def __init__(self, client_factory: Callable[[str], LunarisClient]) -> None:
        self._client_factory = client_factory

    def _client(self, namespace: tuple[str, ...]) -> LunarisClient:
        # namespace → validated Lunaris scope (":" / bad char rejected here).
        return self._client_factory(namespace_to_scope(namespace))

    # ── async surface (the documented adapter API) ───────────────────────────
    async def aput(
        self,
        namespace: tuple[str, ...],
        key: str,
        value: dict[str, Any],
        index: Any = None,
        *,
        ttl: Any = None,
    ) -> None:
        client = self._client(namespace)
        await client.ingest(
            source=f"langgraph:{key}",
            content=_extract_text(value),
            metadata={"key": key, "namespace": list(namespace), "value": value},
        )

    async def aget(
        self, namespace: tuple[str, ...], key: str, *, refresh_ttl: Any = None
    ) -> Item | None:
        # Lunaris is a semantic/recall store, not a KV — best-effort key lookup
        # via recall(query=key, k=1). Returns None when nothing surfaces.
        client = self._client(namespace)
        hits = await client.recall(key, k=1)
        if not hits:
            return None
        return _item(namespace, key, hits[0])

    async def asearch(
        self,
        namespace: tuple[str, ...],
        query: str,
        *,
        limit: int = 10,
    ) -> list[SearchItem]:
        client = self._client(namespace)
        hits = await client.recall(query, k=limit)
        return [_search_item(namespace, h) for h in hits]

    # ── required abstract methods (dispatch to the async surface) ────────────
    async def abatch(self, ops: list[Any]) -> list[Any]:
        results: list[Any] = []
        for op in ops:
            if isinstance(op, PutOp):
                if op.value is None:
                    # delete is unsupported on a semantic store — no-op.
                    results.append(None)
                else:
                    await self.aput(op.namespace, op.key, op.value)
                    results.append(None)
            elif isinstance(op, GetOp):
                results.append(await self.aget(op.namespace, op.key))
            elif isinstance(op, SearchOp):
                results.append(
                    await self.asearch(
                        op.namespace_prefix, op.query or "", limit=op.limit
                    )
                )
            else:
                # ListNamespacesOp / unknown — not supported by this adapter.
                results.append(None)
        return results

    def batch(self, ops: list[Any]) -> list[Any]:
        return asyncio.run(self.abatch(ops))


def _item(namespace: tuple[str, ...], key: str, hit: Hit) -> Item:
    return Item(
        namespace=namespace,
        key=key,
        value={"content": hit.content, "id": hit.id, "source": hit.source},
        created_at=_now(),
        updated_at=_now(),
    )


def _search_item(namespace: tuple[str, ...], hit: Hit) -> SearchItem:
    return SearchItem(
        namespace=namespace,
        key=hit.id,
        value={"content": hit.content, "id": hit.id, "source": hit.source},
        created_at=_now(),
        updated_at=_now(),
        score=hit.score,
    )
