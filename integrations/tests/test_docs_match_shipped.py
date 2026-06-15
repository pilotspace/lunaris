"""RED-first doc-lint: shipped-adapter claims must match reality.

Framework-free, package-free — reads doc files directly (mirrors the
mem0-docs-reconcile validate_*.py approach). At RED these FAIL because the
stale "v0.4 ecosystem" overclaim + "(roadmap)" framing still stand.

Coordinated with the `mem0-docs-reconcile` task (§0 note) to avoid
double-editing the same doc claim.
"""
from __future__ import annotations

import pytest

_ZEP_DOCS = ["docs/MIGRATING-FROM-ZEP.md", "docs/book/src/migrating/zep.md"]
_POSITIONING_DOCS = [
    "docs/POSITIONING.md",
    "docs/book/src/getting-started/why-lunaris.md",
]


@pytest.mark.parametrize("rel", _ZEP_DOCS)
def test_no_stale_v04_ecosystem_overclaim(repo_root, rel):
    text = (repo_root / rel).read_text(encoding="utf-8")
    assert "v0.4 ecosystem" not in text, (
        f"{rel} still carries the stale 'v0.4 ecosystem' overclaim"
    )


@pytest.mark.parametrize("rel", _POSITIONING_DOCS)
def test_adapters_named_as_shipped(repo_root, rel):
    text = (repo_root / rel).read_text(encoding="utf-8")
    low = text.lower()
    for fw in ("langgraph", "crewai", "letta"):
        assert fw in low, f"{rel} does not name the {fw} adapter"
    # The adapter line must no longer be framed as future-only roadmap.
    assert "not yet shipped" not in low, (
        f"{rel} still says the adapters are 'not yet shipped'"
    )
    assert "ecosystem (roadmap)" not in low, (
        f"{rel} still frames the adapters as '(roadmap)'"
    )
