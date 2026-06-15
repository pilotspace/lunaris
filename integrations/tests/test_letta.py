"""RED-first: Letta `LunarisArchivalConnector` maps insert/search.

`pytest.importorskip("letta")` — missing extra SKIPs. Per the freeze flag,
Letta is the riskiest of the three: if the pinned connector base cannot be
subclassed cleanly the deliverable DEGRADES to a thin client-backed shim PLUS a
recipe doc. Either way the shim's insert→ingest / search→recall mapping is
tested, and the recipe doc must exist.
"""
from __future__ import annotations

import pytest

pytest.importorskip("letta")

from lunaris_integrations.client import Hit, StubLunarisClient
from lunaris_integrations.letta import LunarisArchivalConnector


# ── Scenario: Letta connector maps insert/search (or degrades to a recipe) ────
def test_letta_connector_maps(repo_root):
    stub = StubLunarisClient(
        scope="letta_a", hits=[Hit("1", "Acme is a company", 0.7, "doc")]
    )
    connector = LunarisArchivalConnector(stub)

    connector.insert("Alice joined Acme")
    results = connector.search("Acme", top_k=4)

    assert len(stub.ingest_calls) == 1
    assert "Alice joined Acme" in stub.ingest_calls[0][1]
    assert stub.recall_calls == [("Acme", 4)]
    assert results  # recalled passages surfaced

    # Degrade-safe deliverable: the recipe doc ships regardless.
    recipe = repo_root / "examples" / "letta-lunaris" / "README.md"
    assert recipe.exists(), "Letta recipe doc must ship (degrade path)"
