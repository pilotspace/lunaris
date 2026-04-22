"""Phase 11 Plan 11-03 Task 2 — Python documentary parity driver.

Python-driver leg of the cross-language parity suite. Mirrors the 5
Rust-driver scenarios from
`crates/lunaris-recipes/tests/documentary_parity.rs`:

  1. DocumentKnowledgeBase — basic RAG, query "quickstart"
  2. ResearchPaperCorpus — graph-off recall, query "reciprocal rank
     fusion"
  3. CodeRepoMemory — `TemporalQuery.as_of(commit_50_ts)` marquee
     scenario (Phase 11 SC #3)
  4. TimelineReconstruction — `.between(2025-01-10, 2025-01-16)` returns
     exactly 6 events (lower-inclusive, upper-exclusive per Phase 9.1)
  5. CustomerSupportHistory — "refund" recall preserves `ticket:` +
     `chat:` source prefixes with unique `(source, id)` pairs

Per-driver backend parity (Phase 8 CONTEXT — Moon rows == Postgres
rows WITHIN the Python run) is the correct interpretation of Phase 11
SC #5. Top-k SET equality (D-13 tie-bucket ordering accepted) is the
cross-backend assertion inside this one Python process.

Fixtures are loaded from `crates/lunaris-recipes/tests/fixtures/
documentary/*.json` via relative path; we never duplicate fixture data
in the Python crate. Golden is loaded from the same tree.

Skip discipline mirrors `test_backend_parity.py`: if either
`LUNARIS_MOON_URL` or `LUNARIS_POSTGRES_URL` is unset / unreachable,
the test skips cleanly (no hard failure). If both are unreachable the
test skips neutrally.

Imports: the documentary wrappers are exposed as the `lunaris.documentary`
submodule by Plan 11-02b's PyModule routing
(`lunaris-py/src/lib.rs:109-112`). The compiled cdylib lives at
`lunaris.lunaris` (pyproject `module-name = "lunaris.lunaris"`), so the
Rust-registered submodule surfaces as `lunaris.lunaris.documentary`.
Both `from lunaris.lunaris.documentary import CodeRepoMemory` and the
namespace access pattern below are supported.
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


# -----------------------------------------------------------------------------
# Paths — everything anchors on the repo checkout layout.
#   crates/lunaris-py/tests/test_documentary_parity.py   (this file)
#   crates/lunaris-recipes/tests/fixtures/documentary/*.json
# Walk up 3 levels (tests → lunaris-py → crates) then descend.
# -----------------------------------------------------------------------------
_FIXTURES_ROOT = (
    pathlib.Path(__file__).resolve().parents[2]
    / "lunaris-recipes"
    / "tests"
    / "fixtures"
    / "documentary"
)
_GOLDEN_PATH = _FIXTURES_ROOT / "parity_golden.json"


def _load_golden() -> dict:
    if not _GOLDEN_PATH.exists():
        pytest.skip(
            f"parity_golden.json missing at {_GOLDEN_PATH} — run from the "
            f"lunaris repo checkout (submodule-init / re-clone if needed)"
        )
    return json.loads(_GOLDEN_PATH.read_text())


def _load_fixture(name: str) -> Any:
    path = _FIXTURES_ROOT / name
    if not path.exists():
        pytest.skip(f"fixture missing at {path}")
    return json.loads(path.read_text())


# -----------------------------------------------------------------------------
# Skip helpers — two-tier (env + TCP probe) per Plan 04-03 / 05-02 pattern.
# -----------------------------------------------------------------------------
def _parse_host_port(url: str) -> tuple[str, int] | None:
    for scheme, default_port in (
        ("moon://", 6379),
        ("postgres://", 5432),
        ("postgresql://", 5432),
    ):
        if url.startswith(scheme):
            rest = url[len(scheme):]
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


def _reachable(host: str, port: int, timeout_s: float = 1.0) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout_s):
            return True
    except (OSError, ValueError):
        return False


def _probe_backend(env_name: str) -> str | None:
    url = os.environ.get(env_name)
    if not url:
        print(
            f"documentary_parity: SKIP {env_name} (env var unset)",
            file=sys.stderr,
        )
        return None
    parsed = _parse_host_port(url)
    if parsed is None:
        print(
            f"documentary_parity: SKIP {env_name} (unknown URL scheme)",
            file=sys.stderr,
        )
        return None
    host, port = parsed
    if not _reachable(host, port):
        # Do not log the full URL (postgres:// may carry credentials).
        print(
            f"documentary_parity: SKIP {env_name} (TCP probe to {host}:{port} failed)",
            file=sys.stderr,
        )
        return None
    return url


def _backends_or_skip() -> tuple[str, str]:
    moon = _probe_backend("LUNARIS_MOON_URL")
    pg = _probe_backend("LUNARIS_POSTGRES_URL")
    if moon is None or pg is None:
        pytest.skip(
            "documentary_parity needs BOTH LUNARIS_MOON_URL and "
            "LUNARIS_POSTGRES_URL reachable — per-driver backend parity "
            "requires two live backends"
        )
    return moon, pg


def _lunaris_module():
    """Import the `documentary` submodule exposed by `lunaris` package.

    Per Plan 10-03 commit `4747946`, `lunaris/python/__init__.py`
    promotes the cdylib submodule into the package namespace so
    `from lunaris.documentary import X` works (matches Plan 11-03
    must_haves §1). Fall back to the raw cdylib path
    `lunaris.lunaris.documentary` for wheels built from pre-10-03
    checkouts.
    """
    try:
        import lunaris  # noqa: F401
    except ImportError as e:
        pytest.skip(f"lunaris Python bindings not installed: {e}")

    doc_mod = None
    try:
        from lunaris import documentary as doc_mod  # type: ignore[attr-defined]
    except (ImportError, AttributeError):
        try:
            from lunaris.lunaris import documentary as doc_mod  # type: ignore[attr-defined]
        except (ImportError, AttributeError) as e:
            pytest.skip(
                f"lunaris.documentary / lunaris.lunaris.documentary "
                f"submodule not present (rebuild with `maturin develop "
                f"--release` against Plan 11-02b + 10-03): {e}"
            )
    required = (
        "DocumentKnowledgeBase",
        "ResearchPaperCorpus",
        "CodeRepoMemory",
        "TimelineReconstruction",
        "CustomerSupportHistory",
    )
    for cls in required:
        if not hasattr(doc_mod, cls):
            pytest.skip(
                f"lunaris.lunaris.documentary.{cls} not found — rebuild "
                f"required against Plan 11-02b surface.toml"
            )
    return doc_mod


def _rfc3339_to_unix_ms(s: str) -> int:
    """Mirror of `documentary_parity.rs::rfc3339_to_unix_ms`.

    Accepts exactly `YYYY-MM-DDTHH:MM:SSZ` (20 chars). Rejects fractional
    seconds by shape check.
    """
    if len(s) != 20 or not s.endswith("Z") or s[10] != "T":
        raise ValueError(f"unsupported RFC3339 shape: {s!r}")
    y, mo, d = int(s[0:4]), int(s[5:7]), int(s[8:10])
    h, mi, se = int(s[11:13]), int(s[14:16]), int(s[17:19])
    # Howard Hinnant civil → days.
    y_adj = y - 1 if mo <= 2 else y
    era = y_adj // 400 if y_adj >= 0 else -((-y_adj + 399) // 400)
    yoe = y_adj - era * 400
    doy = (153 * (mo - 3 if mo > 2 else mo + 9) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    days_from_civil = era * 146097 + doe - 719468
    unix_seconds = days_from_civil * 86400 + h * 3600 + mi * 60 + se
    return unix_seconds * 1000


# -----------------------------------------------------------------------------
# Scenario 1 — DocumentKnowledgeBase basic RAG.
# -----------------------------------------------------------------------------
async def _run_kb_quickstart(
    doc_mod: Any, url: str, backend_label: str, query: str, top_k: int
) -> list[tuple[str, str]]:
    import lunaris

    mem = await lunaris.open(url)
    prefix = f"kb:docs/doc-11-03-py/{backend_label}/"
    kb = doc_mod.DocumentKnowledgeBase(mem, prefix)
    docs = _load_fixture("document_knowledge_base_docs.json")
    for d in docs:
        meta = {"doc_id": d["id"], "title": d["title"]}
        await kb.ingest([(d["body"], meta)])
    hits = await kb.top(top_k).search(query)
    return [(h["source"], h["text"]) for h in hits]


def test_document_knowledge_base_parity_quickstart_rag():
    doc_mod = _lunaris_module()
    moon, pg = _backends_or_skip()
    golden = _load_golden()
    scenario = golden["scenarios"]["document_knowledge_base_basic_rag"]

    async def run():
        moon_hits = await _run_kb_quickstart(
            doc_mod, moon, "moon", scenario["query"], scenario["top_k"]
        )
        pg_hits = await _run_kb_quickstart(
            doc_mod, pg, "pg", scenario["query"], scenario["top_k"]
        )
        moon_set = set(moon_hits)
        pg_set = set(pg_hits)
        assert moon_set == pg_set, (
            f"DocumentKnowledgeBase 'quickstart' set divergence:\n"
            f"  moon={moon_set}\n  pg={pg_set}"
        )
        assert len(moon_hits) >= scenario["expected_min_hits"], (
            f"expected ≥{scenario['expected_min_hits']} hits; got {len(moon_hits)}"
        )
        needles = scenario["expected_hit_body_contains_any"]
        assert any(
            any(n in body for n in needles) for _s, body in moon_hits
        ), f"expected body match in {needles}; got {moon_hits}"

    asyncio.run(run())


# -----------------------------------------------------------------------------
# Scenario 2 — ResearchPaperCorpus graph-off recall.
# -----------------------------------------------------------------------------
async def _run_research_paper(
    doc_mod: Any, url: str, backend_label: str, query: str
) -> list[tuple[str, str]]:
    import lunaris

    mem = await lunaris.open(url)
    prefix = f"papers:doc-11-03-py/{backend_label}/"
    corpus = doc_mod.ResearchPaperCorpus(mem, prefix)
    corpus = corpus.with_graph_pipeline(False)
    papers = _load_fixture("research_paper_corpus_papers.json")
    for p in papers:
        body = f"{p['title']}\n\n{p['abstract']}"
        meta = {"paper_id": p["id"], "title": p["title"]}
        await corpus.ingest([(body, meta)])
    hits = await corpus.search(query)
    return [(h["source"], h["text"]) for h in hits]


def test_research_paper_corpus_parity_graph_off_recall():
    doc_mod = _lunaris_module()
    moon, pg = _backends_or_skip()
    golden = _load_golden()
    scenario = golden["scenarios"]["research_paper_corpus_graph_off"]

    async def run():
        moon_hits = await _run_research_paper(
            doc_mod, moon, "moon", scenario["query"]
        )
        pg_hits = await _run_research_paper(doc_mod, pg, "pg", scenario["query"])
        moon_set = set(moon_hits)
        pg_set = set(pg_hits)
        assert moon_set == pg_set, (
            f"ResearchPaperCorpus '{scenario['query']}' set divergence:\n"
            f"  moon={moon_set}\n  pg={pg_set}"
        )
        assert len(moon_hits) >= scenario["expected_min_hits"]
        needles = scenario["expected_hit_body_contains_any"]
        assert any(any(n in body for n in needles) for _s, body in moon_hits)

    asyncio.run(run())


# -----------------------------------------------------------------------------
# Scenario 3 — CodeRepoMemory TemporalQuery .as_of(commit_50) — Phase 11 SC #3.
# -----------------------------------------------------------------------------
async def _run_code_repo_as_of(
    doc_mod: Any,
    url: str,
    backend_label: str,
    query: str,
    commit_index_0based: int,
) -> list[str]:
    import lunaris

    mem = await lunaris.open(url)
    prefix = f"repo:doc-11-03-py/{backend_label}/"
    repo = doc_mod.CodeRepoMemory(mem, prefix)
    commits = _load_fixture("code_repo_100_commits.json")
    target = commits[commit_index_0based]
    target_ms = _rfc3339_to_unix_ms(target["committer_date_rfc3339"])
    # Hlc JSON shape as emitted by the napi-rs / PyO3 codegen: `{wall_ms,
    # counter, node_id}`. `recall(query, as_of)` param is a Hlc JSON
    # object.
    as_of = {"wall_ms": target_ms, "counter": 0, "node_id": 0}

    for c in commits:
        ms = _rfc3339_to_unix_ms(c["committer_date_rfc3339"])
        meta = {"function_name": "target"}
        await repo.ingest_commit(c["sha"], ms, [(c["function_body_chunk"], meta)])

    hits = await repo.recall(query, as_of)
    return [h["text"] for h in hits]


def test_code_repo_memory_parity_as_of_commit_50():
    doc_mod = _lunaris_module()
    moon, pg = _backends_or_skip()
    golden = _load_golden()
    scenario = golden["scenarios"]["code_repo_memory_as_of_commit_50"]

    async def run():
        moon_texts = await _run_code_repo_as_of(
            doc_mod, moon, "moon",
            scenario["query"], scenario["commit_index_0based"],
        )
        pg_texts = await _run_code_repo_as_of(
            doc_mod, pg, "pg",
            scenario["query"], scenario["commit_index_0based"],
        )
        moon_set = set(moon_texts)
        pg_set = set(pg_texts)
        assert moon_set == pg_set, (
            f"CodeRepoMemory .as_of(commit_{scenario['commit_index_0based'] + 1}) "
            f"set divergence:\n  moon={moon_set}\n  pg={pg_set}"
        )
        assert len(moon_texts) >= scenario["expected_min_hits"]
        expected = scenario["expected_first_body_contains"]
        assert any(expected in t for t in moon_texts), (
            f"moon: expected `{expected}` in hits; got {moon_texts}"
        )
        assert any(expected in t for t in pg_texts), (
            f"pg: expected `{expected}` in hits; got {pg_texts}"
        )

    asyncio.run(run())


# -----------------------------------------------------------------------------
# Scenario 4 — TimelineReconstruction .between exactly 6 events.
# -----------------------------------------------------------------------------
async def _run_timeline_between(
    doc_mod: Any,
    url: str,
    backend_label: str,
    query: str,
    lo_rfc3339: str,
    hi_rfc3339: str,
) -> list[str]:
    import lunaris

    mem = await lunaris.open(url)
    prefix = f"timeline:doc-11-03-py/{backend_label}/"
    timeline = doc_mod.TimelineReconstruction(mem, prefix)
    events = _load_fixture("timeline_30_days.json")
    for e in events:
        ms = _rfc3339_to_unix_ms(e["valid_time_rfc3339"])
        meta = {"event_id": e["id"], "event_valid_time_unix_ms": ms}
        await timeline.ingest([(e["text"], meta)])
    lo_ms = _rfc3339_to_unix_ms(lo_rfc3339)
    hi_ms = _rfc3339_to_unix_ms(hi_rfc3339)
    lo = {"wall_ms": lo_ms, "counter": 0, "node_id": 0}
    hi = {"wall_ms": hi_ms, "counter": 0, "node_id": 0}
    hits = await timeline.between(query, lo, hi)
    return [h["text"] for h in hits]


def test_timeline_reconstruction_parity_between_10_and_15():
    doc_mod = _lunaris_module()
    moon, pg = _backends_or_skip()
    golden = _load_golden()
    scenario = golden["scenarios"]["timeline_reconstruction_between_10_and_15"]

    async def run():
        moon_texts = await _run_timeline_between(
            doc_mod, moon, "moon",
            scenario["query"],
            scenario["between_lo_rfc3339"],
            scenario["between_hi_rfc3339"],
        )
        pg_texts = await _run_timeline_between(
            doc_mod, pg, "pg",
            scenario["query"],
            scenario["between_lo_rfc3339"],
            scenario["between_hi_rfc3339"],
        )
        moon_set = set(moon_texts)
        pg_set = set(pg_texts)
        assert moon_set == pg_set, (
            f"TimelineReconstruction .between set divergence:\n"
            f"  moon={moon_set}\n  pg={pg_set}"
        )
        assert len(moon_texts) == scenario["expected_count"], (
            f"expected exactly {scenario['expected_count']} events in "
            f"[{scenario['between_lo_rfc3339']}, "
            f"{scenario['between_hi_rfc3339']}) (inclusive-lo / exclusive-hi); "
            f"got {len(moon_texts)}: {moon_texts}"
        )
        for needle in scenario["expected_event_ids_slice"]:
            assert any(needle in t for t in moon_texts), (
                f"expected `{needle}` in moon hits; got {moon_texts}"
            )

    asyncio.run(run())


# -----------------------------------------------------------------------------
# Scenario 5 — CustomerSupportHistory refund recall + bucket isolation.
# -----------------------------------------------------------------------------
async def _run_customer_support_refund(
    doc_mod: Any, url: str, query: str
) -> list[tuple[str, bytes]]:
    import lunaris

    mem = await lunaris.open(url)
    hist = doc_mod.CustomerSupportHistory(mem)
    fx = _load_fixture("customer_support_50_tickets.json")
    for t in fx["tickets"]:
        await hist.ingest_ticket(t["id"], t["body"])
    for c in fx["chats"]:
        await hist.ingest_chat(
            c["ticket_id"], c["turn_idx"], c["participant"], c["msg"]
        )
    hits = await hist.recall(query)
    # Hit JSON shape: {id: bytes-or-str, source: str, text: str, ...}.
    return [(h["source"], h["id"]) for h in hits]


def test_customer_support_history_parity_refund_recall():
    doc_mod = _lunaris_module()
    moon, pg = _backends_or_skip()
    golden = _load_golden()
    scenario = golden["scenarios"]["customer_support_refund_recall"]

    async def run():
        for label, url in (("moon", moon), ("pg", pg)):
            hits = await _run_customer_support_refund(doc_mod, url, scenario["query"])
            prefixes = scenario["expected_source_prefixes"]
            ticket_prefix, chat_prefix = prefixes[0], prefixes[1]
            tickets = [h for h in hits if h[0].startswith(ticket_prefix)]
            chats = [h for h in hits if h[0].startswith(chat_prefix)]
            assert len(tickets) >= scenario["expected_min_ticket_hits"], (
                f"{label}: expected ≥{scenario['expected_min_ticket_hits']} "
                f"`{ticket_prefix}` hits; got {len(tickets)}"
            )
            assert len(chats) >= scenario["expected_min_chat_hits"], (
                f"{label}: expected ≥{scenario['expected_min_chat_hits']} "
                f"`{chat_prefix}` hits; got {len(chats)}"
            )
            if scenario["expected_unique_source_ids"]:
                # Coerce `id` to bytes (may come back as list[int] or
                # bytes depending on PyO3 emitter).
                def norm(x: Any) -> tuple[str, bytes]:
                    s, i = x
                    if isinstance(i, (bytes, bytearray)):
                        return (s, bytes(i))
                    if isinstance(i, (list, tuple)):
                        return (s, bytes(i))
                    return (s, str(i).encode("utf-8"))

                unique = {norm(h) for h in hits}
                assert len(unique) == len(hits), (
                    f"{label}: duplicate (source, id) pairs — RRF bucket "
                    f"isolation broken: {hits}"
                )

    asyncio.run(run())


# -----------------------------------------------------------------------------
# Offline sanity — golden JSON loads and has the 5 expected scenarios.
# Runs without live backends so a misformed golden surfaces on every
# invocation of `pytest crates/lunaris-py/tests/test_documentary_parity.py`.
# -----------------------------------------------------------------------------
def test_parity_golden_json_loads_with_expected_scenarios():
    golden = _load_golden()
    assert golden["schema_version"] == 1
    assert golden["seed"] == "lunaris-doc-parity-v1"
    for key in (
        "document_knowledge_base_basic_rag",
        "research_paper_corpus_graph_off",
        "code_repo_memory_as_of_commit_50",
        "timeline_reconstruction_between_10_and_15",
        "customer_support_refund_recall",
    ):
        assert key in golden["scenarios"], f"golden missing scenario `{key}`"
