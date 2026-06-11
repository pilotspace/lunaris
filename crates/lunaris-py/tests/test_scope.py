"""Wave 3G — Scope / EpisodeBuilder / ScopedLunaris binding tests.

Tests are split into two groups:

1. **Offline** — Scope validation + EpisodeBuilder construction. These run on
   any machine, no backend required.
2. **Online** — ingest under a scope + cross-scope isolation. These require a
   live Moon backend (LUNARIS_TEST_MOON_URL; see conftest.py) and skip when
   none is reachable.
"""
from __future__ import annotations

import pytest
import lunaris


# ---------------------------------------------------------------------------
# Offline: Scope validation
# ---------------------------------------------------------------------------


def test_scope_valid_construction() -> None:
    """Valid scope strings must construct without error."""
    s = lunaris.Scope("agent.alpha")
    assert s.as_str() == "agent.alpha"
    assert str(s) == "agent.alpha"


def test_scope_repr() -> None:
    s = lunaris.Scope("acme.agent-42")
    assert "acme.agent-42" in repr(s)


def test_scope_equality() -> None:
    a = lunaris.Scope("agent.alpha")
    b = lunaris.Scope("agent.alpha")
    c = lunaris.Scope("agent.beta")
    assert a == b
    assert a != c


def test_scope_rejects_colon() -> None:
    """`:` must be rejected (KV-aliasing defense, v0.2.1 alphabet).

    `:` is the delimiter of the lunaris:{scope}:{kind}:{ulid} KV format.
    `Scope::new` rejects it at the type level so no scope string can
    byte-alias another scope's keyspace. A spec asserting `:` valid is
    asserting a security regression.
    """
    with pytest.raises(ValueError, match="[Ss]cope"):
        lunaris.Scope("acme:agent-42")


def test_scope_rejects_empty() -> None:
    with pytest.raises(ValueError, match="[Ss]cope"):
        lunaris.Scope("")


def test_scope_rejects_129_chars() -> None:
    """A 129-character string must be rejected (limit is 128)."""
    with pytest.raises(ValueError, match="[Ss]cope"):
        lunaris.Scope("a" * 129)


def test_scope_accepts_128_chars() -> None:
    """A 128-character string (the exact limit) must be accepted."""
    s = lunaris.Scope("a" * 128)
    assert len(s.as_str()) == 128


def test_scope_rejects_space() -> None:
    with pytest.raises(ValueError, match="[Ss]cope"):
        lunaris.Scope("has space")


def test_scope_rejects_slash() -> None:
    with pytest.raises(ValueError, match="[Ss]cope"):
        lunaris.Scope("has/slash")


def test_scope_allows_all_valid_chars() -> None:
    """All characters in [A-Za-z0-9_\\-.] must be accepted."""
    lunaris.Scope("A0._-")


# ---------------------------------------------------------------------------
# Offline: EpisodeBuilder
# ---------------------------------------------------------------------------


def test_episode_builder_basic() -> None:
    b = lunaris.EpisodeBuilder("src/report.md", "hello world")
    assert repr(b) == "EpisodeBuilder(...)"


def test_episode_builder_t_ref() -> None:
    b = lunaris.EpisodeBuilder("src", "content").t_ref("2026-01-01T00:00:00Z")
    assert b is not None


def test_episode_builder_t_ref_invalid() -> None:
    with pytest.raises(ValueError, match="[Ii]SO"):
        lunaris.EpisodeBuilder("src", "content").t_ref("not-a-date")


def test_episode_builder_metadata() -> None:
    b = lunaris.EpisodeBuilder("src", "content").metadata({"author": "helios"})
    assert b is not None


def test_episode_builder_chaining() -> None:
    b = (
        lunaris.EpisodeBuilder("src", "content")
        .t_ref("2026-05-11T00:00:00Z")
        .metadata({"k": "v"})
    )
    assert b is not None


# ---------------------------------------------------------------------------
# Online: ScopedLunaris ingest + cross-scope isolation
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_scoped_ingest_returns_lsn(moon_backend_url: str) -> None:
    """engine.scoped(scope).ingest(builder) must return an LSN string."""
    handle = await lunaris.open(moon_backend_url)
    scope = lunaris.Scope("agent.alpha")
    builder = lunaris.EpisodeBuilder(
        "py-test/scope",
        "the quick brown fox jumps over the lazy dog",
    )
    scoped = handle.scoped(scope)
    lsn = await scoped.ingest(builder)
    assert isinstance(lsn, str)
    assert ":" in lsn  # Lsn Display format is "{wall_ms}:{counter}"


@pytest.mark.asyncio
async def test_scoped_ingest_with_metadata(moon_backend_url: str) -> None:
    """Metadata and t_ref fields must be accepted without error."""
    handle = await lunaris.open(moon_backend_url)
    scope = lunaris.Scope("agent.alpha")
    builder = (
        lunaris.EpisodeBuilder("py-test/meta", "some content")
        .t_ref("2026-01-01T00:00:00Z")
        .metadata({"source_type": "unit-test"})
    )
    lsn = await handle.scoped(scope).ingest(builder)
    assert isinstance(lsn, str)


@pytest.mark.asyncio
async def test_cross_scope_isolation(moon_backend_url: str) -> None:
    """Content ingested under scope_a must not appear in scope_b recall.

    This is the canonical cross-scope isolation assertion for RFC 0001.

    Implementation note: we ingest a unique sentinel string under scope_a,
    then recall under scope_b with the same query. A correct implementation
    returns an empty hit list for scope_b. We use a unique enough string to
    avoid false positives from leftover data in a shared dev Moon instance.
    """
    import time

    handle = await lunaris.open(moon_backend_url)
    unique = f"lunaris-wave3g-isolation-{time.time_ns()}"

    scope_a = lunaris.Scope("wave3g.scope-a")
    scope_b = lunaris.Scope("wave3g.scope-b")

    # Ingest under scope_a.
    builder = lunaris.EpisodeBuilder("py-test/isolation", unique)
    await handle.scoped(scope_a).ingest(builder)

    # Recall under scope_b — should return no hits for our unique string.
    hits_b = await handle.scoped(scope_b).recall(unique)
    assert isinstance(hits_b, list)
    # Filter hits to only those whose content contains our unique sentinel.
    matching = [
        h for h in hits_b
        if unique in str(h.get("content", "") if isinstance(h, dict) else h)
    ]
    assert matching == [], (
        f"Cross-scope leak detected: scope_b returned {len(matching)} hit(s) "
        f"for content ingested under scope_a"
    )


@pytest.mark.asyncio
async def test_scoped_lunaris_scope_getter(moon_backend_url: str) -> None:
    """scoped.scope must return the bound Scope object."""
    handle = await lunaris.open(moon_backend_url)
    scope = lunaris.Scope("agent.alpha")
    scoped = handle.scoped(scope)
    assert scoped.scope.as_str() == "agent.alpha"


@pytest.mark.asyncio
async def test_scoped_dsl_returns_retrieval_builder(moon_backend_url: str) -> None:
    """scoped.dsl() must return a RetrievalBuilder (not None, not raise)."""
    handle = await lunaris.open(moon_backend_url)
    scope = lunaris.Scope("agent.alpha")
    scoped = handle.scoped(scope)
    builder = scoped.dsl()
    assert builder is not None
