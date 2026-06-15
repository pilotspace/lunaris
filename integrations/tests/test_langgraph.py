"""RED-first: LangGraph `LunarisStore` maps BaseStore ops to ingest/recall.

`pytest.importorskip("langgraph")` — a missing extra SKIPs (not fails); the
BUILD env installs langgraph so the discriminating mapping test actually runs.
At RED (langgraph absent) these SKIP; the framework-free client/scope tests
carry the RED signal.
"""
from __future__ import annotations

import asyncio

import pytest

pytest.importorskip("langgraph")

from lunaris_integrations.client import Hit, StubLunarisClient
from lunaris_integrations.langgraph import LunarisStore


def _run(coro):
    return asyncio.run(coro)


# ── Scenario: LangGraph LunarisStore maps store ops to ingest/recall ──────────
def test_langgraph_store_maps():
    stub = StubLunarisClient(
        scope="u.mem", hits=[Hit("01H", "Alice joined Acme", 0.9, "chat")]
    )
    seen_scopes: list[str] = []

    def factory(scope: str) -> StubLunarisClient:
        seen_scopes.append(scope)
        return stub

    store = LunarisStore(factory)

    async def body():
        await store.aput(("u", "mem"), "k1", {"text": "Alice joined Acme"})
        return await store.asearch(("u", "mem"), "where did Alice join")

    results = _run(body())

    # ingest scoped by namespace_to_scope(("u","mem")) == "u.mem"
    assert seen_scopes and all(s == "u.mem" for s in seen_scopes)
    assert len(stub.ingest_calls) == 1
    _src, content, _meta = stub.ingest_calls[0]
    assert "Alice joined Acme" in content  # the value travelled into ingest

    # asearch → one recall, mapped to LangGraph item shape (carries score+value)
    assert len(stub.recall_calls) == 1
    assert len(results) == 1
    assert results[0].score == 0.9
    assert "Alice joined Acme" in str(results[0].value)


# ── Scenario: the SAME adapter is transport-agnostic ──────────────────────────
def test_transport_agnostic():
    # Two stubs tagged http/sdk; the SAME LunarisStore code path must produce
    # identical client call shapes — it holds a LunarisClient, never branches.
    def drive(tag: str):
        stub = StubLunarisClient(scope="u.mem", hits=[Hit("1", "x", 1.0, "s")])
        stub.tag = tag  # transport marker the adapter must ignore
        store = LunarisStore(lambda scope: stub)

        async def body():
            await store.aput(("u", "mem"), "k1", {"text": "hello"})
            await store.asearch(("u", "mem"), "hello?")

        _run(body())
        return stub.ingest_calls, stub.recall_calls

    http_shape = drive("http")
    sdk_shape = drive("sdk")
    assert http_shape == sdk_shape
