"""Phase 10 Plan 10-03 — Python cross-language parity for conversational wrappers.

Drives each of the 5 Phase 10 conversational wrappers end-to-end from
the Python binding layer through both Moon and Postgres, re-using the
same canonical JSON fixtures landed by Plan 10-01 under
`crates/lunaris-recipes/tests/fixtures/conversational/`. Per-driver
backend parity — direct Moon-vs-Postgres hit-id ordering compare —
mirrors the scope-reset Phase 8 Plan 08-04 convention AND the Plan
10-01 Rule 1 deviation (see 10-01-SUMMARY.md §2). There is NO
pre-computed SHA-256 lock in the fixtures and NO cross-language
byte-identity assertion — each driver self-verifies per-backend
parity against its own fresh ingest.

Skip posture (matches `conftest.py::moon_backend_url` + Plan 08-04
`test_backend_parity.py`):

- Both `LUNARIS_TEST_MOON_URL` + `LUNARIS_TEST_POSTGRES_URL` must be
  set AND TCP-reachable for the live-ingest path to fire.
- When either is unset/unreachable the test exits cleanly (pytest
  `skip`) — default `uv run pytest` on a fresh checkout stays green.
- When the Moon handshake trips on plain-Redis (`FT.CREATE` missing)
  we skip gracefully, matching the `_probe_handshake` cache idiom in
  `conftest.py`.

Scope-isolation degradation (Rule 1 deviation from Plan 10-03 text):

The plan asks for a `subscribe_audit_events()` + `ConsolidatorPromotion`
counter on the Python binding. That API is NOT in the Phase 8 / 11-02b
surface. The Rust-side test (`conversational_parity.rs` Task 2)
installs a deterministic `TestConsolidator` via
`lunaris.consolidator_pipeline().set_consolidator(...)` — also not
exposed through PyO3 in v0.1.1. The Python test therefore degrades
to the shape Rust uses when there is no custom consolidator:
**Moon + Postgres `.consolidate()` reports must have equal
`promotions` / `archives` counts**. Semantic scope-filter correctness
is already proven on the Rust side (Phase 10 SC #5 closed in 10-01);
this test closes the "Python caller exercises the same code path"
leg of Phase 10 SC #5.

Directory convention: sibling Py tests under `crates/lunaris-py/tests/`
follow the `test_*.py` naming. Plan 10-03 text calls the file
`conversational_parity.py` (no `test_` prefix). We ship BOTH — the
file name matches the plan spec and a module-level `def test_*`
function per scenario satisfies pytest's default collector.
"""

from __future__ import annotations

import asyncio
import json
import os
import pathlib
import socket
import sys
from typing import Any

import pytest

# Canonical fixture path — reach from this file back to
# `crates/lunaris-recipes/tests/fixtures/conversational/`. Structure:
#   crates/lunaris-py/tests/conversational_parity.py  (this file)
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


def _parse_host_port(url: str) -> tuple[str, int] | None:
    """Mirror of `conftest.py::_parse_moon_host_port` plus Postgres scheme.

    Also handles `postgres://user:pw@host:port/db` userinfo stripping to
    avoid printing credentials on skip.
    """
    for scheme, default_port in (
        ("moon://", 6379),
        ("postgres://", 5432),
        ("postgresql://", 5432),
    ):
        if url.startswith(scheme):
            rest = url[len(scheme) :]
            if "@" in rest:
                rest = rest.rsplit("@", 1)[1]
            authority = rest.split("/")[0].split("?")[0]
            if ":" in authority:
                host, port_str = authority.split(":", 1)
                try:
                    return host, int(port_str)
                except ValueError:
                    return None
            return authority, default_port
    return None


def _tcp_reachable(host: str, port: int, timeout_s: float = 1.0) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout_s):
            return True
    except (OSError, ValueError):
        return False


def _probe_backend(env_name: str) -> str | None:
    """Two-tier probe — mirror of the Plan 04-03 / 08-04 `probe_backend` shape.

    Returns the URL to use, or `None` to skip. Logs the skip reason to
    stderr (without the URL, which may carry credentials) so CI surfaces
    the cause.
    """
    url = os.environ.get(env_name)
    if not url:
        print(
            f"conversational_parity: SKIP {env_name} (env var unset)",
            file=sys.stderr,
        )
        return None
    parsed = _parse_host_port(url)
    if parsed is None:
        print(
            f"conversational_parity: SKIP {env_name} (unknown URL scheme)",
            file=sys.stderr,
        )
        return None
    host, port = parsed
    if not _tcp_reachable(host, port):
        print(
            f"conversational_parity: SKIP {env_name} (TCP probe to {host}:{port} failed)",
            file=sys.stderr,
        )
        return None
    return url


def _hit_key(hit: Any) -> tuple[Any, Any]:
    """Stable ordering key on a pythonize'd `Hit` — `(source, id)`.

    `hit["id"]` is `list[int]` (serde `Vec<u8>` → JSON array of bytes),
    `hit["source"]` is `str`. Both are comparable and byte-faithful.
    """
    if isinstance(hit, dict):
        return (hit.get("source", ""), tuple(hit.get("id", [])))
    # Opaque napi-rs-style object fallback — rare; Phase 8 binds hits via
    # pythonize so dict is the primary path.
    return (getattr(hit, "source", ""), tuple(getattr(hit, "id", [])))


def _compare_hits(label: str, moon: list[Any], pg: list[Any]) -> list[str]:
    """Return a list of divergence strings; empty list == parity."""
    divs: list[str] = []
    if len(moon) != len(pg):
        divs.append(f"{label}: hit_count moon={len(moon)} postgres={len(pg)}")
    for i, (m, p) in enumerate(zip(moon, pg)):
        mk = _hit_key(m)
        pk = _hit_key(p)
        if mk != pk:
            divs.append(
                f"{label} position {i}: id/source moon={mk!r} postgres={pk!r}"
            )
    return divs


def _require_both_backends() -> tuple[str, str]:
    """Probe both backends; skip the test when either is missing.

    Plan-spec parity requires BOTH — skipping one and testing only the
    other would violate the "per-driver backend parity" contract.
    """
    moon = _probe_backend("LUNARIS_TEST_MOON_URL")
    pg = _probe_backend("LUNARIS_TEST_POSTGRES_URL")
    if moon is None or pg is None:
        pytest.skip(
            "conversational_parity requires BOTH LUNARIS_TEST_MOON_URL + "
            "LUNARIS_TEST_POSTGRES_URL to be set and reachable"
        )
    return moon, pg


async def _open_pair(moon_url: str, pg_url: str) -> tuple[Any, Any]:
    """Open both handles; skip if either fails the handshake (plain
    Redis without RediSearch trips `FT.CREATE`). Mirror of
    `conftest.py::_probe_handshake` but inline per-test so each scenario
    gets a fresh handle.
    """
    import lunaris

    try:
        moon = await lunaris.open(moon_url)
    except Exception as e:
        pytest.skip(f"Moon handshake failed: {type(e).__name__}: {e}")
    try:
        pg = await lunaris.open(pg_url)
    except Exception as e:
        pytest.skip(f"Postgres handshake failed: {type(e).__name__}: {e}")
    return moon, pg


# ----------------------------------------------------------------------
# Scenario 1: ChatAgentMemory — 10-turn chat round-trip parity
# ----------------------------------------------------------------------
def test_chat_agent_memory_parity() -> None:
    from lunaris.conversational import ChatAgentMemory

    moon_url, pg_url = _require_both_backends()
    fixture = _load_fixture("chat_agent_memory.json")
    user_id = fixture["user_id"]
    turns = fixture["turns"]
    query = fixture["query"]

    async def run() -> list[str]:
        moon, pg = await _open_pair(moon_url, pg_url)
        cam_moon = ChatAgentMemory.new(moon, user_id)
        cam_pg = ChatAgentMemory.new(pg, user_id)
        for turn in turns:
            text = turn["text"]
            await cam_moon.remember(text)
            await cam_pg.remember(text)
        moon_hits = await cam_moon.recall(query)
        pg_hits = await cam_pg.recall(query)
        return _compare_hits("chat_agent_memory", moon_hits, pg_hits)

    divs = asyncio.run(run())
    assert not divs, f"ChatAgentMemory Moon↔Postgres parity violation: {divs}"


# ----------------------------------------------------------------------
# Scenario 2: MultiTurnConversation — recall + consolidate parity
# ----------------------------------------------------------------------
def test_multi_turn_conversation_parity() -> None:
    from lunaris.conversational import MultiTurnConversation

    moon_url, pg_url = _require_both_backends()
    fixture = _load_fixture("multi_turn_conversation.json")
    user_id = fixture["user_id"]
    other_user_id = fixture["other_user_id"]
    sessions = fixture["sessions"]
    other_turns = fixture["other_turns"]
    query = fixture["query"]

    async def run() -> list[str]:
        moon, pg = await _open_pair(moon_url, pg_url)
        conv_moon = MultiTurnConversation.new(moon, user_id)
        conv_pg = MultiTurnConversation.new(pg, user_id)
        other_moon = MultiTurnConversation.new(moon, other_user_id)
        other_pg = MultiTurnConversation.new(pg, other_user_id)

        for session in sessions:
            thread_id = session["thread_id"]
            for turn in session["turns"]:
                text = turn["text"]
                await conv_moon.remember(text, thread_id)
                await conv_pg.remember(text, thread_id)

        # Seed control other-user turns. Rust Task 2 installs a
        # TestConsolidator here to give the scope filter teeth; PyO3
        # binding does not expose `set_consolidator()` in v0.1.1, so
        # we rely on the Rust-side proof of the scope-filter semantics
        # and only assert report-count parity here.
        for turn in other_turns:
            text = turn["text"]
            await other_moon.remember(text, "ctl")
            await other_pg.remember(text, "ctl")

        moon_hits = await conv_moon.recall(query)
        pg_hits = await conv_pg.recall(query)
        divs = _compare_hits("multi_turn_conversation recall", moon_hits, pg_hits)

        moon_report = await conv_moon.consolidate()
        pg_report = await conv_pg.consolidate()

        moon_n = len(moon_report.get("promotions", []) or [])
        pg_n = len(pg_report.get("promotions", []) or [])
        if moon_n != pg_n:
            divs.append(
                f"multi_turn consolidate promotions moon={moon_n} postgres={pg_n}"
            )

        # Scope-isolation from the Python caller. Even with the
        # default-NoopConsolidator path, any promotion that escaped the
        # scope filter would still tag a source starting with
        # `chat:user-other/` — flag it if observed on either backend.
        for report, label in ((moon_report, "moon"), (pg_report, "postgres")):
            for p in report.get("promotions", []) or []:
                # Promotions may not carry source (depends on
                # Consolidator impl) — conservative: skip if absent.
                src = p.get("episode", {}).get("source") if isinstance(p, dict) else None
                if isinstance(src, str) and src.startswith(f"chat:{other_user_id}/"):
                    divs.append(
                        f"{label} consolidate leaked other-user source: {src!r}"
                    )
        return divs

    divs = asyncio.run(run())
    assert not divs, f"MultiTurnConversation parity violation: {divs}"


# ----------------------------------------------------------------------
# Scenario 3: SlackArchive — channel filter parity
# ----------------------------------------------------------------------
def test_slack_archive_channel_filter_parity() -> None:
    from lunaris.conversational import SlackArchive

    moon_url, pg_url = _require_both_backends()
    fixture = _load_fixture("slack_archive.json")
    channels = fixture["channels"]
    query = fixture["query"]
    channel_filter = fixture["channel_filter"]

    async def run() -> list[str]:
        moon, pg = await _open_pair(moon_url, pg_url)
        sl_moon = SlackArchive.new(moon)
        sl_pg = SlackArchive.new(pg)
        for channel in channels:
            ch_id = channel["id"]
            for msg in channel["messages"]:
                user = msg["user"]
                text = msg["text"]
                await sl_moon.ingest_channel(ch_id, user, text)
                await sl_pg.ingest_channel(ch_id, user, text)

        moon_wide = await sl_moon.recall(query)
        pg_wide = await sl_pg.recall(query)
        divs = _compare_hits("slack_archive wide", moon_wide, pg_wide)

        moon_narrow = await sl_moon.channel(channel_filter).recall(query)
        pg_narrow = await sl_pg.channel(channel_filter).recall(query)
        divs += _compare_hits(
            f"slack_archive channel={channel_filter}", moon_narrow, pg_narrow
        )
        return divs

    divs = asyncio.run(run())
    assert not divs, f"SlackArchive parity violation: {divs}"


# ----------------------------------------------------------------------
# Scenario 4: EmailThreading — graph-off default parity
# ----------------------------------------------------------------------
def test_email_threading_graph_off_parity() -> None:
    from lunaris.conversational import EmailThreading

    moon_url, pg_url = _require_both_backends()
    fixture = _load_fixture("email_threading.json")
    root_id = fixture["root_id"]
    messages = fixture["messages"]
    query = fixture["query"]

    async def run() -> list[str]:
        moon, pg = await _open_pair(moon_url, pg_url)
        # Blueprint §5.2 default — graph pipeline off on a fresh handle.
        assert moon.graph_pipeline.is_enabled() is False, (
            "Moon handle must default to graph-off"
        )
        assert pg.graph_pipeline.is_enabled() is False, (
            "Postgres handle must default to graph-off"
        )
        em_moon = EmailThreading.new(moon)
        em_pg = EmailThreading.new(pg)
        for msg in messages:
            await em_moon.ingest(root_id, msg["from"], msg["body"])
            await em_pg.ingest(root_id, msg["from"], msg["body"])

        moon_hits = await em_moon.thread(root_id).recall(query)
        pg_hits = await em_pg.thread(root_id).recall(query)
        divs = _compare_hits("email_threading graph-off", moon_hits, pg_hits)

        # Default flow must NOT flip the graph pipeline on.
        if moon.graph_pipeline.is_enabled():
            divs.append("moon graph pipeline flipped ON during default-flow recall")
        if pg.graph_pipeline.is_enabled():
            divs.append("postgres graph pipeline flipped ON during default-flow recall")
        return divs

    divs = asyncio.run(run())
    assert not divs, f"EmailThreading graph-off parity violation: {divs}"


# ----------------------------------------------------------------------
# Scenario 5: EmailThreading — graph-on opt-in (Gemma-gated)
# ----------------------------------------------------------------------
@pytest.mark.skipif(
    not os.environ.get("LUNARIS_EXTRACT_GEMMA_PATH"),
    reason="graph-on path loads Gemma-3 4B — set LUNARIS_EXTRACT_GEMMA_PATH to opt in",
)
def test_email_threading_graph_on_opt_in() -> None:
    from lunaris.conversational import EmailThreading

    moon_url, pg_url = _require_both_backends()
    # pg_url not exercised on this path — graph-on opt-in only needs one
    # handle to validate the toggle-surface wiring (risk row #2 opt-in
    # scope). Keep the dual probe so skip semantics match the other
    # scenarios.
    _ = pg_url

    async def run() -> None:
        import lunaris

        moon = await lunaris.open(moon_url)
        assert moon.graph_pipeline.is_enabled() is False
        em = EmailThreading.new(moon).with_graph_pipeline(True)
        assert moon.graph_pipeline.is_enabled() is True, (
            "with_graph_pipeline(True) must enable the pipeline"
        )
        _ = em.with_graph_pipeline(False)
        assert moon.graph_pipeline.is_enabled() is False, (
            "with_graph_pipeline(False) must disable the pipeline"
        )

    asyncio.run(run())


# ----------------------------------------------------------------------
# Scenario 6: MeetingNotesMemory — 10-note transcript parity
# ----------------------------------------------------------------------
def test_meeting_notes_memory_parity() -> None:
    from lunaris.conversational import MeetingNotesMemory

    moon_url, pg_url = _require_both_backends()
    fixture = _load_fixture("meeting_notes_memory.json")
    notes = fixture["notes"]
    query = fixture["query"]

    async def run() -> list[str]:
        moon, pg = await _open_pair(moon_url, pg_url)
        mn_moon = MeetingNotesMemory.new(moon)
        mn_pg = MeetingNotesMemory.new(pg)
        for n in notes:
            heading = n["heading"]
            body = n["body"]
            await mn_moon.note(heading, body)
            await mn_pg.note(heading, body)

        moon_hits = await mn_moon.recall(query)
        pg_hits = await mn_pg.recall(query)
        return _compare_hits("meeting_notes_memory", moon_hits, pg_hits)

    divs = asyncio.run(run())
    assert not divs, f"MeetingNotesMemory parity violation: {divs}"


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
    out from under us. A grep assert in CI would also catch this; this
    runs it at pytest collection time.
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
