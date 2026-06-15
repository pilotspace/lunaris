"""RED-first: CrewAI `LunarisCrewAIStorage` maps save/search/reset.

`pytest.importorskip("crewai")` — missing extra SKIPs. save/search/reset are
SYNC (the CrewAI Storage interface); the adapter drives the async client
internally.
"""
from __future__ import annotations

import pytest

pytest.importorskip("crewai")

from lunaris_integrations.client import Hit, StubLunarisClient
from lunaris_integrations.crewai import LunarisCrewAIStorage


# ── Scenario: CrewAI LunarisCrewAIStorage maps save/search/reset ──────────────
def test_crewai_storage_maps():
    stub = StubLunarisClient(
        scope="crew_a", hits=[Hit("1", "Acme is a company", 0.8, "chat")]
    )
    storage = LunarisCrewAIStorage(stub)

    storage.save("Alice joined Acme", {"agent": "a"})
    results = storage.search("Acme", limit=3)
    storage.reset()

    # save → one ingest carrying the value + metadata
    assert len(stub.ingest_calls) == 1
    _src, content, meta = stub.ingest_calls[0]
    assert content == "Alice joined Acme"
    assert meta == {"agent": "a"}

    # search → one recall(k=limit) returning CrewAI result dicts
    assert stub.recall_calls == [("Acme", 3)]
    assert isinstance(results, list) and results
    assert all(isinstance(r, dict) for r in results)
    assert any("Acme" in str(r) for r in results)

    # reset → forget/scope-clear
    assert stub.forget_calls == 1
