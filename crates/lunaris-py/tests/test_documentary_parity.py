"""Phase 11 Plan 11-03 Task 2 — Python documentary parity driver.

Python-driver leg of the cross-language parity suite. The 5 scenarios
originally mirrored a Rust driver at
`crates/lunaris-recipes/tests/documentary_parity.rs` — that file was
DELETED in commit `03bf8bc` and only its fixtures survive, so the
"three independent drivers (Rust / Python / TypeScript)" that
`conformance-bindings.yml`'s header still advertises is now two.
The scenarios:

  1. DocumentKnowledgeBase — basic RAG, query "quickstart"
  2. ResearchPaperCorpus — graph-off recall, query "reciprocal rank
     fusion"
  3. CodeRepoMemory — `TemporalQuery.as_of(commit_50_ts)` marquee
     scenario (Phase 11 SC #3)
  4. TimelineReconstruction — `.between(2025-01-10, 2025-01-16)` returns
     exactly 6 events (lower-inclusive, upper-exclusive per Phase 9.1)
  5. CustomerSupportHistory — "refund" recall preserves `ticket:` +
     `chat:` source prefixes with unique `(source, id)` pairs

What these tests assert (0.7.0 — CORRECTED, ship-plan W2.9)
-----------------------------------------------------------
Each scenario runs ONCE against a live Moon and asserts its rows
against the committed golden. That is the whole contract now.

Until W2.9 this file called itself a *backend* parity driver: every
scenario ran twice — once on `LUNARIS_MOON_URL`, once on
`LUNARIS_POSTGRES_URL` — and the headline assertion was top-k SET
equality between the two (D-13 tie-bucket ordering accepted). 0.7.0
deleted `lunaris-storage-postgres`; `lunaris.open` now rejects every
scheme but `moon://` with `UnsupportedScheme`. So the Postgres leg
could not have run even against a live Postgres server, and the
`_backends_or_skip()` gate — which demanded BOTH URLs — silently
skipped all five scenarios on every invocation, including in
`conformance-bindings.yml`, which stands up a real Moon on 6391 and
runs this file. Five live-Moon scenarios were being thrown away to
satisfy a dead second backend.

The cross-backend SET-equality assertion is GONE and is not
recoverable — there is no second backend to compare against. What
survives is per-driver golden conformance, which is what
`conformance-bindings.yml`'s own header describes as the intent
("Each driver asserts its own rows against the committed golden
reference"). The Rust / Python / TypeScript drivers each check
themselves against the same golden; cross-LANGUAGE byte-identity was
always explicitly out of scope.

FOUR of the five scenarios now run for real. The fifth
(`code_repo_memory_parity_as_of_commit_50`) is skipped against a NAMED
product gap, not silently: Moon has no KV version chain, so a
system-time `.as_of` pinned 18 months back — which is what the golden
pins — is refused with `NotSupported`. See `_MOON_HISTORICAL_KV_READS`
below for the one-line unskip.

NOTE ON THE NAMES: the `_parity_` infix in the five test names is now
a misnomer. It is kept deliberately — `docs/book/src/cookbook/
document-kb.md` and `research-and-code.md` cite these names, and those
files are outside this change's scope. Rename both together.

Fixtures are loaded from `crates/lunaris-recipes/tests/fixtures/
documentary/*.json` via relative path; we never duplicate fixture data
in the Python crate. Golden is loaded from the same tree.

Skip discipline: if `LUNARIS_MOON_URL` is unset or not TCP-reachable,
the test skips cleanly (no hard failure).

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

from conftest import run_tag, run_window_offset_ms


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
    # `moon://` only. The `postgres://` / `postgresql://` rows that used to
    # sit here died with `lunaris-storage-postgres` in 0.7.0 — `lunaris.open`
    # answers every other scheme with `UnsupportedScheme`, so parsing one
    # would only have produced a URL nothing could open.
    for scheme, default_port in (("moon://", 6379),):
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
        # Log host:port, never the full URL — a store URL may carry credentials.
        print(
            f"documentary_parity: SKIP {env_name} (TCP probe to {host}:{port} failed)",
            file=sys.stderr,
        )
        return None
    return url


# Mirror of `lunaris_storage_moon::as_of::HISTORICAL_KV_READS`.
#
# Moon has no KV version chain, so `StoragePort::read_as_of` refuses any pin
# older than `AS_OF_LIVE_WINDOW_MS` (1 h) with `StorageError::NotSupported`.
# That makes the system-time `.as_of` scenario below unrunnable on the only
# backend 0.7.0 ships. Flip this to True on the day the Rust constant flips,
# and the scenario starts gating again.
_MOON_HISTORICAL_KV_READS = False


def _moon_or_skip() -> str:
    """The one live backend. Skips — never silently passes — when absent."""
    moon = _probe_backend("LUNARIS_MOON_URL")
    if moon is None:
        pytest.skip(
            "documentary_parity needs LUNARIS_MOON_URL set and reachable "
            "(moon:// is the only scheme lunaris.open accepts since 0.7.0)"
        )
    return moon


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
# F34 — every source prefix below carries a per-RUN token.
#
# Without it this suite is only correct against a FRESH backend: each helper
# re-ingests its fixtures under a constant prefix, so a second run against the
# same Moon reads its own rows plus the previous run's. Measured on
# `origin/main`: `test_timeline_reconstruction_parity_between_10_and_15`
# returned 12 events where it asserts exactly 6, every one duplicated. CI never
# saw it because runners are fresh — which makes this a "CI gets away with it"
# pass, not a "CI proves it is fine" one.
#
# The token keeps the EXACT count, which is the sharpest assertion in the file
# and the thing that pins `.between`'s inclusive-lo / exclusive-hi boundary.
# Relaxing it to a lower bound would have made the symptom go away and stopped
# detecting a genuine over-return at the same time.
#
# `test_conversational_parity.py` has always done this; `run_tag` now lives in
# conftest so there is one definition rather than a copy per suite.
_TAG = run_tag()

# W4.17 — ONE scope per suite run. This is the axis Moon actually partitions
# on, applied BELOW any top-k, so it isolates this run from every earlier run
# and from the TypeScript twin in a way a source prefix cannot (F34: the
# timeline recipe post-filters its prefix AFTER the root's global `top(30)`).
_SUITE_SCOPE = f"pydoc-{_TAG}"

# ...and the timeline scenario additionally shifts its VALID-TIME window per
# run. The prefix alone does not isolate it: `TimelineReconstruction.between`
# pushes `@valid_time:[lo hi]` down to Moon but filters the prefix in memory
# AFTER the root's global `top(30)`, so every run's 6 in-window rows compete
# for the same 30 slots. Measured against one accumulating store: run 2
# returned 12, run 5 returned exactly 30 (the cap), and once the TypeScript
# twin piled onto the same window a later run returned 5 of its own 6 — an
# UNDER-return, which reads like a product bug rather than a dirty store.
# Shifting the window makes Moon's own numeric filter do the isolating.
_WINDOW_OFFSET_MS = run_window_offset_ms()


async def _run_kb_quickstart(
    doc_mod: Any, url: str, backend_label: str, query: str, top_k: int
) -> list[tuple[str, str]]:
    import lunaris

    mem = await lunaris.open(url)
    prefix = f"kb:docs/doc-11-03-py/{backend_label}/{_TAG}/"
    kb = doc_mod.DocumentKnowledgeBase.new(mem, _SUITE_SCOPE, prefix)
    docs = _load_fixture("document_knowledge_base_docs.json")
    for d in docs:
        meta = {"doc_id": d["id"], "title": d["title"]}
        await kb.ingest([(d["body"], meta)])
    hits = await kb.top(top_k).search(query)
    return [(h["source"], h["text"]) for h in hits]


def test_document_knowledge_base_parity_quickstart_rag():
    doc_mod = _lunaris_module()
    moon = _moon_or_skip()
    golden = _load_golden()
    scenario = golden["scenarios"]["document_knowledge_base_basic_rag"]

    async def run():
        moon_hits = await _run_kb_quickstart(
            doc_mod, moon, "moon", scenario["query"], scenario["top_k"]
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
    prefix = f"papers:doc-11-03-py/{backend_label}/{_TAG}/"
    corpus = doc_mod.ResearchPaperCorpus.new(mem, _SUITE_SCOPE, prefix)
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
    moon = _moon_or_skip()
    golden = _load_golden()
    scenario = golden["scenarios"]["research_paper_corpus_graph_off"]

    async def run():
        moon_hits = await _run_research_paper(
            doc_mod, moon, "moon", scenario["query"]
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
    prefix = f"repo:doc-11-03-py/{backend_label}/{_TAG}/"
    repo = doc_mod.CodeRepoMemory.new(mem, _SUITE_SCOPE, prefix)
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
    # BLOCKED BY A PRODUCT GAP, not by the harness — and it is unblocked by
    # fixing Moon, not by editing this file.
    #
    # `CodeRepoMemory.recall(q, as_of)` is `TemporalQuery::<Documents>::as_of`,
    # which sets SYSTEM-time as_of on the RetrievalBuilder. `lunaris-retrieve`
    # hydrate.rs hands that straight to `StoragePort::read_as_of`, and
    # `lunaris-storage-moon` answers any pin older than
    # `AS_OF_LIVE_WINDOW_MS` (1 h) with `StorageError::NotSupported` —
    # `HISTORICAL_KV_READS = false` in `crates/lunaris-storage-moon/src/as_of.rs`,
    # because Moon has no KV version chain. This scenario's golden pins
    # `as_of = 2025-02-19T12:00:00Z`, roughly 18 months back, so the call
    # raises rather than returning rows. Moon is the only backend since 0.7.0,
    # so there is nowhere for it to pass.
    #
    # It is skipped rather than deleted because the fixture and the golden are
    # still correct — this is the scenario that would prove bi-temporal
    # time-travel through the SDK the day the KV version chain lands (Moon
    # carries a half-built `TemporalKvIndex` with no production call sites).
    # It is skipped rather than left running because a test known to fail is
    # not a gate, it is a broken build.
    #
    # UNSKIP WHEN: `lunaris_storage_moon::as_of::HISTORICAL_KV_READS` is
    # true — flip _MOON_HISTORICAL_KV_READS (defined above) to match it.
    if not _MOON_HISTORICAL_KV_READS:
        pytest.skip(
            "0.7.0 product gap: Moon refuses historical KV reads "
            "(HISTORICAL_KV_READS = false; as_of pinned ~18 months back by "
            "the golden), so TemporalQuery.as_of raises NotSupported. Unskip "
            "when HISTORICAL_KV_READS flips true."
        )
    doc_mod = _lunaris_module()
    moon = _moon_or_skip()
    golden = _load_golden()
    scenario = golden["scenarios"]["code_repo_memory_as_of_commit_50"]

    async def run():
        moon_texts = await _run_code_repo_as_of(
            doc_mod, moon, "moon",
            scenario["query"], scenario["commit_index_0based"],
        )
        assert len(moon_texts) >= scenario["expected_min_hits"], (
            f"expected ≥{scenario['expected_min_hits']} hits; got {len(moon_texts)}"
        )
        expected = scenario["expected_first_body_contains"]
        assert any(expected in t for t in moon_texts), (
            f"moon: expected `{expected}` in hits; got {moon_texts}"
        )

    asyncio.run(run())


# W4.13 — the REFUSAL is the contract, and nothing asserted it.
#
# The scenario above is skipped against a named product gap, which is the right
# call: a test known to fail is a broken build, not a gate. But skipping it left
# the SDK-level time-travel story documented and untested in BOTH directions —
# nothing checked that a historical `as_of` returns rows, and nothing checked
# that it refuses either. An `as_of` that silently returned an empty list, or
# that quietly answered with latest-state rows, would have passed every test in
# this repo.
#
# That second failure mode is the dangerous one. Returning today's rows for a
# pin 18 months back is a wrong answer that looks like a right one; the whole
# point of `reject_historical_read` is that it refuses BEFORE issuing any RESP
# command, so a rejected read cannot be confused with a transport failure.
#
# The assertion keys on `moon_kv_as_of`, which
# `crates/lunaris-storage-moon/src/as_of.rs` defines as the greppable machine
# token for exactly this purpose, rather than on the prose that follows it.
#
# INVERT WHEN: `HISTORICAL_KV_READS` flips true — at which point this test
# should assert rows come back and the scenario above should be unskipped.
# Both are gated on the same mirrored constant so they cannot drift apart.
def test_historical_as_of_is_refused_not_silently_empty():
    if _MOON_HISTORICAL_KV_READS:
        pytest.skip(
            "HISTORICAL_KV_READS is true — the refusal this asserts no longer "
            "applies. Unskip test_code_repo_memory_parity_as_of_commit_50 and "
            "delete this test in the same commit."
        )
    doc_mod = _lunaris_module()
    moon = _moon_or_skip()
    golden = _load_golden()
    scenario = golden["scenarios"]["code_repo_memory_as_of_commit_50"]

    async def run():
        with pytest.raises(Exception) as excinfo:
            await _run_code_repo_as_of(
                doc_mod, moon, "moon",
                scenario["query"], scenario["commit_index_0based"],
            )
        msg = str(excinfo.value)
        assert "moon_kv_as_of" in msg, (
            "a historical as_of must be REFUSED with the moon_kv_as_of token, "
            "not answered and not failed some other way. lunaris-server maps "
            f"this variant to a 501; got: {msg}"
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
    prefix = f"timeline:doc-11-03-py/{backend_label}/{_TAG}/"
    timeline = doc_mod.TimelineReconstruction.new(mem, _SUITE_SCOPE, prefix)
    events = _load_fixture("timeline_30_days.json")
    for e in events:
        ms = _rfc3339_to_unix_ms(e["valid_time_rfc3339"]) + _WINDOW_OFFSET_MS
        meta = {"event_id": e["id"], "valid_time_unix_ms": ms}
        await timeline.ingest([(e["text"], meta)])
    # The SAME shift on both sides, so the inclusive-lo / exclusive-hi boundary
    # this scenario exists to pin is preserved exactly under the translation.
    lo_ms = _rfc3339_to_unix_ms(lo_rfc3339) + _WINDOW_OFFSET_MS
    hi_ms = _rfc3339_to_unix_ms(hi_rfc3339) + _WINDOW_OFFSET_MS
    lo = {"wall_ms": lo_ms, "counter": 0, "node_id": 0}
    hi = {"wall_ms": hi_ms, "counter": 0, "node_id": 0}
    hits = await timeline.between(query, lo, hi)
    return [h["text"] for h in hits]


# F21 FIXED — the `xfail(strict=True)` marker that used to sit here is gone,
# and the assertions below are the ones it was parked in front of, unchanged.
#
# What it recorded: `TimelineReconstruction.ingest` forwards to
# `DocumentCorpus::ingest`, which built `Episode::new(...)` — `bt:
# BiTemporal::now(clock)`, `t_ref: None` — and stored the caller's valid-time
# as ordinary metadata. `.between(lo, hi)` renders `Filter::ValidTimeRange`
# into Moon's `@valid_time:[lo hi]`, which matched the INGEST time, so a
# corpus of 2025-01 events ingested today reconstructed nothing.
#
# Two things had to change, in two different layers:
#
#   1. Core. The valid axis was not caller-settable ANYWHERE — no production
#      path called `BiTemporal::at`. `Episode::ground_valid_axis` now moves it
#      to `t_ref`, and every chunk inherits `episode.bt.valid.0`.
#   2. Recipe. `DocumentCorpus` honours the reserved metadata key
#      `valid_time_unix_ms` (note: NOT the `event_`-prefixed name this test
#      used to invent — `DocumentCorpus` serves papers, docs and repos too).
#
# A third, separate hole turned up while fixing it: the graph-OFF ingest path
# — the shipped default, and the one every DocumentCorpus recipe takes — never
# wrote a `valid_time_ms` field at all, so `Filter::ValidTimeRange` matched
# nothing regardless of what the axis said.
#
# `strict=True` did its job exactly as designed: fixing the recipe turned this
# test red, which is what forced the marker's removal and these assertions'
# return in the same commit. A `skip` would have read "not run" forever.
def test_timeline_reconstruction_parity_between_10_and_15():
    doc_mod = _lunaris_module()
    moon = _moon_or_skip()
    golden = _load_golden()
    scenario = golden["scenarios"]["timeline_reconstruction_between_10_and_15"]

    async def run():
        moon_texts = await _run_timeline_between(
            doc_mod, moon, "moon",
            scenario["query"],
            scenario["between_lo_rfc3339"],
            scenario["between_hi_rfc3339"],
        )
        # The sharpest assertion in the file: an EXACT count, which pins the
        # lower-inclusive / upper-exclusive `.between` boundary (Phase 9.1).
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
    hist = doc_mod.CustomerSupportHistory.new(mem, _SUITE_SCOPE)

    # W4.17 retired the PRECONDITION that used to stand here.
    #
    # `CustomerSupportHistory` takes no source prefix, and until W4.17 every
    # recipe binding in both SDKs constructed at `Scope::dev()` — so this
    # scenario had NO isolation knob at all: a previous run's 50 tickets sat in
    # the same scope, under the same sources, competing for the same `top(30)`.
    # Rather than fail three steps downstream as "ticket-prefix hits: expected
    # 0 to be greater than or equal to 1" (a dirty store that reads like a
    # recall regression), #184 asserted the requirement out loud.
    #
    # It is a real per-run partition now: `_SUITE_SCOPE` above. The tourniquet
    # came off with the wound. Note what the loud version bought — it turned a
    # silent wrong answer into a red board on `main` within the hour, which is
    # what sent someone to fix the cause.

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
    moon = _moon_or_skip()
    golden = _load_golden()
    scenario = golden["scenarios"]["customer_support_refund_recall"]

    async def run():
        for label, url in (("moon", moon),):
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
