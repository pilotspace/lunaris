"""Phase 10 Plan 10-03 — Python driver for the 5 conversational wrappers.

Drives ChatAgentMemory, MultiTurnConversation, SlackArchive, EmailThreading
and MeetingNotesMemory end-to-end from the PyO3 binding layer against a live
Moon, re-using the canonical JSON fixtures under
`crates/lunaris-recipes/tests/fixtures/conversational/`.

WHAT THESE TESTS ASSERT (0.7.0 — REWRITTEN, ship-plan W4.14)
------------------------------------------------------------
Each scenario runs ONCE against a live Moon and asserts the wrapper's own
contract: that its writes land, that its recall returns them, and that its
source-prefix / filter narrowing does what the recipe documents.

This file had TWO independent reasons to run nothing, and either alone was
enough to make it invisible:

1. It was named `conversational_parity.py`. pytest collects `test_*.py` and
   `*_test.py`; this matched neither, so nothing here executed since it
   landed — not skipped, not reported, absent.
2. Every scenario called `_require_both_backends()`, demanding
   `LUNARIS_TEST_POSTGRES_URL` alongside the Moon URL. 0.7.0 deleted
   `lunaris-storage-postgres` and `lunaris.open` rejects every scheme but
   `moon://`, so that gate could never be satisfied.

Fixing only the name would have converted a silent no-op into a loud
failure, which is why W4.15 left the rename to this task. Both are fixed
here.

The cross-backend comparison is GONE and is not recoverable — there is no
second backend. What replaces it is a per-driver contract, mirroring the
W2.9 rewrite of `test_documentary_parity.py` and the Rust-side
`conversational_parity.rs`.

Skip posture
------------
`LUNARIS_MOON_URL` must be set AND TCP-reachable. When it is not, each
scenario `pytest.skip`s — a bare `return` would report as a pass, which is
the defect this file spent three minor versions demonstrating. When the
Moon handshake trips on plain Redis (`FT.CREATE` missing) we skip too,
matching the `_probe_handshake` idiom in `conftest.py`.

Rerun safety
------------
A live Moon is not torn down between runs, so an assertion on an exact hit
count is a flake with a timer on it — green until the index outgrows the k.
Every scenario that can carry a per-run discriminator does (`user_id`,
`root_id`, `channel` all take a run tag), and the one that cannot
(`MeetingNotesMemory` writes under a fixed `meeting:notes/` prefix) asserts
on content needles and lower bounds, never on equality with a count.

Scope-isolation posture (Rule 1 deviation from Plan 10-03 text)
--------------------------------------------------------------
The plan asks for a `subscribe_audit_events()` + `ConsolidatorPromotion`
counter on the Python binding. That API is not in the Phase 8 / 11-02b
surface, and `consolidator_pipeline().set_consolidator(...)` is not exposed
through PyO3 either — so the binding always runs the default
`NoopConsolidator` and an empty report is legitimate. What is asserted
instead is the invariant that holds regardless of consolidator:
**no promotion may carry another user's source prefix.** Semantic
scope-filter correctness is proven on the Rust side (Phase 10 SC #5); this
closes the "Python caller exercises the same code path" leg.
"""

from __future__ import annotations

import asyncio
import json
import os
import pathlib
import secrets
import socket
import sys
import time
from typing import Any

import pytest

# Canonical fixture path — reach from this file back to
# `crates/lunaris-recipes/tests/fixtures/conversational/`. Structure:
#   crates/lunaris-py/tests/test_conversational_parity.py  (this file)
#   crates/lunaris-recipes/tests/fixtures/conversational/*.json
# so we walk up 3 levels (tests → lunaris-py → crates) then descend.
_FIXTURE_DIR = (
    pathlib.Path(__file__).resolve().parents[2]
    / "lunaris-recipes"
    / "tests"
    / "fixtures"
    / "conversational"
)


def _load_fixture(name: str) -> dict[str, Any]:
    path = _FIXTURE_DIR / name
    if not path.exists():
        pytest.skip(f"fixture missing at {path} — run from lunaris checkout root")
    return json.loads(path.read_text())


def _run_tag() -> str:
    """A per-run discriminator, so a second run cannot read the first's rows."""
    return f"{int(time.time() * 1000):x}{secrets.token_hex(3)}"


def _parse_host_port(url: str) -> tuple[str, int] | None:
    """Mirror of `conftest.py::_parse_moon_host_port`.

    `moon://` is the only scheme `lunaris.open` accepts as of 0.7.0, so it is
    the only one parsed here — a `postgres://` URL must read as "unknown
    scheme", not as a reachable backend.
    """
    if not url.startswith("moon://"):
        return None
    rest = url[len("moon://") :]
    if "@" in rest:
        rest = rest.rsplit("@", 1)[1]
    authority = rest.split("/")[0].split("?")[0]
    if ":" in authority:
        host, port_str = authority.split(":", 1)
        try:
            return host, int(port_str)
        except ValueError:
            return None
    return authority, 6379


def _tcp_reachable(host: str, port: int, timeout_s: float = 1.0) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout_s):
            return True
    except (OSError, ValueError):
        return False


def _probe_moon() -> str | None:
    """Two-tier probe — mirror of the Plan 04-03 / 08-04 `probe_backend` shape.

    Returns the URL to use, or `None` to skip. Logs the reason to stderr
    without the URL, which may carry credentials.
    """
    url = os.environ.get("LUNARIS_MOON_URL")
    if not url:
        print("conversational: SKIP (LUNARIS_MOON_URL unset)", file=sys.stderr)
        return None
    parsed = _parse_host_port(url)
    if parsed is None:
        print("conversational: SKIP (LUNARIS_MOON_URL is not a moon:// URL)", file=sys.stderr)
        return None
    host, port = parsed
    if not _tcp_reachable(host, port):
        print(
            f"conversational: SKIP (TCP probe to {host}:{port} failed)",
            file=sys.stderr,
        )
        return None
    return url


def _require_moon() -> str:
    url = _probe_moon()
    if url is None:
        pytest.skip("conversational requires LUNARIS_MOON_URL set and reachable")
    return url


def _hit_field(hit: Any, name: str, default: Any) -> Any:
    """Read a field off a pythonize'd `Hit`.

    Phase 8 binds hits via pythonize, so `dict` is the primary path; the
    attribute fallback covers an opaque-object binding.
    """
    if isinstance(hit, dict):
        return hit.get(name, default)
    return getattr(hit, name, default)


def _hit_key(hit: Any) -> tuple[Any, Any]:
    """`(source, id)` — `id` is `list[int]` (serde `Vec<u8>` → JSON array)."""
    return (_hit_field(hit, "source", ""), tuple(_hit_field(hit, "id", [])))


def _sources(hits: list[Any]) -> list[str]:
    return [str(_hit_field(h, "source", "")) for h in hits]


def _assert_unique(label: str, hits: list[Any]) -> None:
    keys = [_hit_key(h) for h in hits]
    assert len(set(keys)) == len(keys), f"{label}: duplicate (source, id) pairs"


async def _open_moon(url: str) -> Any:
    """Open a handle; skip if the handshake fails.

    Plain Redis without RediSearch trips `FT.CREATE` — that is a skip, not a
    failure. Mirror of `conftest.py::_probe_handshake`, inline per-test so
    each scenario gets a fresh handle.
    """
    import lunaris

    try:
        return await lunaris.open(url)
    except Exception as e:  # noqa: BLE001 — any open failure is a skip
        pytest.skip(f"Moon handshake failed: {type(e).__name__}: {e}")


# ----------------------------------------------------------------------
# Scenario 1: ChatAgentMemory — 10 turns land under chat:<user>/
# ----------------------------------------------------------------------
def test_chat_agent_memory_round_trip() -> None:
    from lunaris.conversational import ChatAgentMemory

    moon_url = _require_moon()
    fixture = _load_fixture("chat_agent_memory.json")
    # Per-run user id: the recipe derives its source prefix from it, so this
    # is what keeps a second run from reading the first run's turns.
    user_id = f"{fixture['user_id']}-{_run_tag()}"
    turns = fixture["turns"]
    query = fixture["query"]

    async def run() -> list[Any]:
        moon = await _open_moon(moon_url)
        cam = ChatAgentMemory.new(moon, user_id)
        for turn in turns:
            await cam.remember(turn["text"])
        return await cam.recall(query)

    hits = asyncio.run(run())
    assert hits, "recall over 10 seeded turns returned nothing"
    _assert_unique("chat_agent_memory", hits)

    # The contract the recipe documents: every primitive gets the SAME
    # `chat:<user_id>/` prefix (chat_agent_memory.rs:44-48). Because
    # `user_id` is per-run, this doubles as the isolation assertion.
    prefix = f"chat:{user_id}/"
    foreign = [s for s in _sources(hits) if not s.startswith(prefix)]
    assert not foreign, f"hits outside {prefix}: {foreign}"


# ----------------------------------------------------------------------
# Scenario 2: MultiTurnConversation — recall + consolidate stay in-scope
# ----------------------------------------------------------------------
def test_multi_turn_conversation_never_crosses_the_user_boundary() -> None:
    from lunaris.conversational import MultiTurnConversation

    moon_url = _require_moon()
    fixture = _load_fixture("multi_turn_conversation.json")
    tag = _run_tag()
    user_id = f"{fixture['user_id']}-{tag}"
    other_user_id = f"{fixture['other_user_id']}-{tag}"
    sessions = fixture["sessions"]
    other_turns = fixture["other_turns"]
    query = fixture["query"]

    async def run() -> tuple[list[Any], Any]:
        moon = await _open_moon(moon_url)
        conv = MultiTurnConversation.new(moon, user_id)
        other = MultiTurnConversation.new(moon, other_user_id)

        for session in sessions:
            for turn in session["turns"]:
                await conv.remember(turn["text"], session["thread_id"])
        # Control seed. Without it the isolation assertions below hold
        # vacuously — there would be no foreign rows to leak.
        for turn in other_turns:
            await other.remember(turn["text"], "ctl")

        return await conv.recall(query), await conv.consolidate()

    hits, report = asyncio.run(run())
    assert hits, "recall over the seeded sessions returned nothing"
    _assert_unique("multi_turn_conversation", hits)

    own_prefix = f"chat:{user_id}/"
    other_prefix = f"chat:{other_user_id}/"
    leaked = [s for s in _sources(hits) if s.startswith(other_prefix)]
    assert not leaked, f"recall leaked the other user's turns: {leaked}"
    assert all(s.startswith(own_prefix) for s in _sources(hits)), (
        f"every hit must sit under {own_prefix}: {_sources(hits)}"
    )

    # The bindings expose the default NoopConsolidator, so an empty report is
    # legitimate — what must NEVER happen is a promotion carrying the other
    # user's source.
    promotions = _hit_field(report, "promotions", []) or []
    leaked_promotions = []
    for p in promotions:
        episode = _hit_field(p, "episode", {}) or {}
        src = _hit_field(episode, "source", "")
        if isinstance(src, str) and src.startswith(other_prefix):
            leaked_promotions.append(src)
    assert not leaked_promotions, (
        f"consolidate promoted another user's episodes: {leaked_promotions}"
    )


# ----------------------------------------------------------------------
# Scenario 3: SlackArchive — channel narrowing is a subset of wide recall
# ----------------------------------------------------------------------
def test_slack_archive_channel_narrowing_is_a_subset() -> None:
    from lunaris.conversational import SlackArchive

    moon_url = _require_moon()
    fixture = _load_fixture("slack_archive.json")
    tag = _run_tag()
    channels = [{**ch, "id": f"{ch['id']}-{tag}"} for ch in fixture["channels"]]
    query = fixture["query"]
    channel_filter = f"{fixture['channel_filter']}-{tag}"

    async def run() -> tuple[list[Any], list[Any]]:
        moon = await _open_moon(moon_url)
        slack = SlackArchive.new(moon)
        for channel in channels:
            for msg in channel["messages"]:
                await slack.ingest_channel(channel["id"], msg["user"], msg["text"])
        return (
            await slack.recall(query),
            await slack.channel(channel_filter).recall(query),
        )

    wide, narrowed = asyncio.run(run())
    assert wide, "wide recall over the seeded channels returned nothing"
    _assert_unique("slack_archive wide", wide)

    # Every row this recipe writes carries the archive prefix
    # (slack_archive.rs:53). A hit without it means the wide recall escaped
    # its own `Filter::StartsWith`.
    stray = [s for s in _sources(wide) if not s.startswith("slack:archive/")]
    assert not stray, f"wide recall escaped slack:archive/: {stray}"

    # `channel(id)` applies `Filter::Eq {field: "channel"}` at the retrieve
    # layer (D-06). Its result must be a subset of the wide recall — a
    # narrowing that returns a row the wide query did not is a
    # filter-pushdown bug, and the relation holds no matter how much
    # unrelated data an earlier run left behind.
    _assert_unique(f"slack_archive channel={channel_filter}", narrowed)
    assert len(narrowed) <= len(wide), (
        f"channel={channel_filter} returned {len(narrowed)} rows, more than "
        f"the unfiltered recall's {len(wide)}"
    )


# ----------------------------------------------------------------------
# Scenario 4: EmailThreading — thread narrowing, graph pipeline stays off
# ----------------------------------------------------------------------
def test_email_threading_thread_narrowing_and_graph_off_default() -> None:
    from lunaris.conversational import EmailThreading

    moon_url = _require_moon()
    fixture = _load_fixture("email_threading.json")
    root_id = f"{fixture['root_id']}-{_run_tag()}"
    messages = fixture["messages"]
    query = fixture["query"]

    async def run() -> list[Any]:
        moon = await _open_moon(moon_url)
        # Blueprint §5.2 default — graph pipeline OFF on a fresh handle.
        assert moon.graph_pipeline.is_enabled() is False, (
            "a fresh handle must be graph-off"
        )
        email = EmailThreading.new(moon)
        for msg in messages:
            await email.ingest(root_id, msg["from"], msg["body"])
        hits = await email.thread(root_id).recall(query)
        assert moon.graph_pipeline.is_enabled() is False, (
            "recall must not switch the graph pipeline on behind the caller's back"
        )
        return hits

    hits = asyncio.run(run())
    assert hits, "thread recall over the seeded messages returned nothing"
    _assert_unique("email_threading", hits)

    # `thread(root_id)` filters on the `email:thread/<root_id>/` source
    # prefix (email_threading.rs:30, 72). Because `root_id` is per-run, this
    # doubles as the isolation assertion.
    prefix = f"email:thread/{root_id}/"
    foreign = [s for s in _sources(hits) if not s.startswith(prefix)]
    assert not foreign, f"thread recall escaped {prefix}: {foreign}"


# ----------------------------------------------------------------------
# Scenario 5: EmailThreading — the graph toggle, both directions
# ----------------------------------------------------------------------
def test_email_threading_graph_toggle_both_directions() -> None:
    """No extractor is loaded here — this exercises the toggle surface only.

    This test used to be `skipif`'d on `LUNARIS_EXTRACT_GEMMA_PATH`, an env
    var no workflow sets and which the v0.6 llama.cpp-only cutover retired.
    Flipping a boolean never needed a model.
    """
    from lunaris.conversational import EmailThreading

    moon_url = _require_moon()

    async def run() -> None:
        moon = await _open_moon(moon_url)
        assert moon.graph_pipeline.is_enabled() is False
        em = EmailThreading.new(moon).with_graph_pipeline(True)
        assert moon.graph_pipeline.is_enabled() is True, (
            "with_graph_pipeline(True) must enable the pipeline"
        )
        em.with_graph_pipeline(False)
        assert moon.graph_pipeline.is_enabled() is False, (
            "with_graph_pipeline(False) must disable the pipeline"
        )

    asyncio.run(run())


# ----------------------------------------------------------------------
# Scenario 6: MeetingNotesMemory — notes land under meeting:notes/
# ----------------------------------------------------------------------
def test_meeting_notes_memory_round_trip() -> None:
    from lunaris.conversational import MeetingNotesMemory

    moon_url = _require_moon()
    fixture = _load_fixture("meeting_notes_memory.json")
    notes = fixture["notes"]
    query = fixture["query"]

    # This is the one wrapper with no per-run discriminator: `note()` writes
    # under the fixed `meeting:notes/` prefix (meeting_notes_memory.rs:28).
    # So the seeded body carries a run tag rather than the assertion carrying
    # a count a previous run has already inflated.
    tag = _run_tag()

    async def run() -> list[Any]:
        moon = await _open_moon(moon_url)
        mn = MeetingNotesMemory.new(moon)
        for n in notes:
            await mn.note(n["heading"], f"{n['body']} [run {tag}]")
        return await mn.recall(query)

    hits = asyncio.run(run())
    assert hits, "recall over the seeded notes returned nothing"
    _assert_unique("meeting_notes_memory", hits)
    stray = [s for s in _sources(hits) if not s.startswith("meeting:notes/")]
    assert not stray, f"recall escaped meeting:notes/: {stray}"


# ----------------------------------------------------------------------
# Offline sanity — fixture path resolution + import surface
# ----------------------------------------------------------------------
def test_conversational_surface_imports_offline() -> None:
    """Offline shape check — every promised class resolves via both
    import paths.

    Does NOT need a backend; guards against 11-02b's Py re-export
    accidentally regressing under a future refactor.
    """
    # Promised-by-plan path.
    from lunaris.conversational import (
        ChatAgentMemory,
        EmailThreading,
        MeetingNotesMemory,
        MultiTurnConversation,
        SlackArchive,
    )
    # Cdylib-internal path — must keep working (binding-surface artefact).
    from lunaris.lunaris import conversational as c  # type: ignore[attr-defined]

    for cls in (
        ChatAgentMemory,
        MultiTurnConversation,
        SlackArchive,
        EmailThreading,
        MeetingNotesMemory,
    ):
        assert hasattr(c, cls.__name__), (
            f"{cls.__name__} missing from lunaris.lunaris.conversational"
        )


def test_fixtures_on_disk() -> None:
    """Offline — guards against Plan 10-01's fixture dir being moved
    out from under us.
    """
    expected = {
        "chat_agent_memory.json",
        "multi_turn_conversation.json",
        "slack_archive.json",
        "email_threading.json",
        "meeting_notes_memory.json",
    }
    found = {p.name for p in _FIXTURE_DIR.glob("*.json")}
    missing = expected - found
    assert not missing, f"fixture files missing: {missing}"
