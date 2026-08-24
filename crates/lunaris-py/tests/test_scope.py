"""Wave 3G — Scope / EpisodeBuilder / ScopedLunaris binding tests.

Tests are split into two groups:

1. **Offline** — Scope validation + EpisodeBuilder construction. These run on
   any machine, no backend required.
2. **Online** — ingest under a scope + cross-scope isolation. These require a
   live Moon backend (LUNARIS_MOON_URL; see conftest.py) and skip when
   none is reachable.
"""
from __future__ import annotations

import uuid

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
async def test_scoped_dsl_returns_a_builder_that_can_be_composed(
    moon_backend_url: str,
) -> None:
    """scoped.dsl() must return a builder that can actually be driven.

    `assert builder is not None` was the whole assertion here until W4.12, and
    it passed against the codegen-frozen stub whose surface is
    `['and', 'as_of', 'filter', 'fuse_rrf', 'top']` — no `query`, no
    `execute`. It proved only that `dsl()` returned an object. The TypeScript
    twin (`expect(builder).toBeDefined()`) had the same hole.
    """
    handle = await lunaris.open(moon_backend_url)
    scope = lunaris.Scope("agent.alpha")
    scoped = handle.scoped(scope)
    builder = scoped.dsl()
    for name in ("query", "top", "execute"):
        assert callable(getattr(builder, name, None)), (
            f"scoped.dsl() returned a builder with no callable `{name}` — "
            f"this is the frozen codegen stub, not the working builder"
        )


# ---------------------------------------------------------------------------
# W4.17 — recipes are partitionable
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_a_recipe_binds_its_partition(moon_backend_url: str) -> None:
    """Two recipe instances on different scopes must not see each other.

    Both use the SAME `user_id` on purpose. A recipe's source prefix
    (`chat:<user>/`) is its OTHER discriminator, and if the two instances
    differed there the test would pass on the prefix alone and prove nothing
    about the scope. Same user, different scope: the partition key is the only
    thing that can separate them.

    Before W4.17 every recipe binding in both SDKs minted `Scope::dev()`, so
    there was no scope argument to pass and this test could not be written.
    """
    from lunaris.conversational import ChatAgentMemory

    tag = uuid.uuid4().hex[:10]
    user = f"w417-{tag}"
    handle = await lunaris.open(moon_backend_url)

    # Tagged so a previous run's rows — different scope, SAME store — cannot
    # satisfy either assertion.
    a_text = f"Alice loves chocolate cake ({tag})."
    b_text = f"Bob also loves chocolate cake ({tag})."

    cam_a = ChatAgentMemory.new(handle, f"w417a-{tag}", user)
    cam_b = ChatAgentMemory.new(handle, f"w417b-{tag}", user)
    await cam_a.remember(a_text)
    await cam_b.remember(b_text)

    # CONTROL first, through the native scoped path (not the recipe). It reads
    # the SAME partition with the SAME query, so it separates "this build has
    # no usable embedder" from "the recipe is bound to the wrong partition".
    # A bare `if not texts: skip` cannot tell those apart, and the second is
    # exactly the defect this test exists to catch.
    control = await handle.scoped(lunaris.Scope(f"w417a-{tag}")).recall(
        "chocolate cake"
    )
    control_texts = [
        h.get("text", "") if isinstance(h, dict) else "" for h in control
    ]
    if a_text not in control_texts:
        pytest.skip(
            "the control recall could not see its own row either — no usable "
            f"embedder in this build; control returned {control_texts}"
        )

    hits = await cam_a.recall("chocolate cake")
    texts = [h.get("text", "") if isinstance(h, dict) else "" for h in hits]
    # POSITIVE: scope a's own row. An instance bound to the WRONG partition
    # still returns other rows, so exclusion alone would pass while reading
    # somebody else's data.
    assert a_text in texts, (
        f"the recipe did not return its own scope's row — it is bound to some "
        f"other partition: {texts}"
    )
    # NEGATIVE: b_text is deliberately near-identical, so it outranks
    # everything else in the store for this query.
    assert b_text not in texts, (
        f"the recipe escaped its partition and returned scope b's row: {texts}"
    )


@pytest.mark.asyncio
async def test_a_recipe_refuses_an_invalid_scope(moon_backend_url: str) -> None:
    """A bad scope string must be refused at construction, loudly.

    `:` is the KV-format delimiter and is rejected by the scope alphabet, so
    accepting it here would let one scope byte-alias another's keyspace.
    """
    from lunaris.conversational import ChatAgentMemory

    handle = await lunaris.open(moon_backend_url)
    with pytest.raises(Exception) as excinfo:
        ChatAgentMemory.new(handle, "w417:colon", "user-1")
    assert "scope" in str(excinfo.value).lower(), (
        f"the refusal must name the scope so the caller knows which argument "
        f"was wrong; got {excinfo.value!r}"
    )
