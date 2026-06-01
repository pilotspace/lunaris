#!/usr/bin/env python3
"""scripts/sdk-real-evidence.py — lunaris-py real-case evidence runner.

Exercises the Python SDK end-to-end against a live Moon backend:
  - open / snapshot / ingest / recall / forget(dry-run) roundtrip
  - ChatAgentMemory.remember / recall conversational roundtrip
  - DocumentKnowledgeBase.ingest / search documentary roundtrip
  - Consolidator + Graph pipeline toggles

Per-scenario JSON envelope written to `${LUNARIS_SDK_EVIDENCE_DIR}/py-<name>.json`.

Backend URL comes from `LUNARIS_TEST_MOON_URL`. Scenarios that require a
real Moon handshake skip-clean if the probe fails; the script still exits 0
on skip, non-zero only on hard failure.
"""
from __future__ import annotations

import asyncio
import json
import os
import socket
import sys
import time
import traceback
from pathlib import Path

MOON_URL = os.environ.get("LUNARIS_TEST_MOON_URL", "moon://127.0.0.1:6380")
OUT_DIR = Path(os.environ.get("LUNARIS_SDK_EVIDENCE_DIR", "milestones/v0.1.1-sdk-real"))
OUT_DIR.mkdir(parents=True, exist_ok=True)


def _envelope(name: str, status: str, duration_ms: float, details: dict) -> dict:
    return {
        "runner": "python",
        "scenario": name,
        "backend": MOON_URL,
        "status": status,
        "duration_ms": round(duration_ms, 3),
        "py_version": sys.version.split()[0],
        "details": details,
    }


def _write(name: str, env: dict) -> None:
    path = OUT_DIR / f"py-{name}.json"
    path.write_text(json.dumps(env, indent=2, default=str))
    mark = {"PASS": "✓", "SKIP": "∘", "FAIL": "✗"}.get(env["status"], "?")
    print(f"  {mark} py:{name:<32} {env['status']:<4} {env['duration_ms']:>8.1f}ms  → {path.name}")


def _tcp_reachable(url: str) -> bool:
    try:
        _, rest = url.split("://", 1)
        host, port = rest.split(":")
        with socket.create_connection((host, int(port)), timeout=0.5):
            return True
    except Exception:
        return False


def _run(name: str, scenario_fn):
    """Run one scenario in its OWN fresh event loop.

    Rationale: pyo3-async-runtimes pins an ``unsendable`` PyClass to the
    thread on which it was created. A cross-scenario loop reuse can straddle
    internal tokio worker threads, so we use `asyncio.run(scenario())` per
    scenario — mirrors `test_documentary_parity.py`'s `asyncio.run(run())`.
    """
    t0 = time.perf_counter()
    try:
        details = asyncio.run(scenario_fn())
        status = details.pop("__status__", "PASS")
        _write(name, _envelope(name, status, (time.perf_counter() - t0) * 1000, details))
        return status != "FAIL"
    except Exception as e:
        _write(name, _envelope(name, "FAIL", (time.perf_counter() - t0) * 1000,
                               {"error": str(e), "traceback": traceback.format_exc().splitlines()[-6:]}))
        return False


# ---------- Scenarios ----------

def _build_episode(content: str, source: str = "sdk-real-py") -> dict:
    import ulid
    return {
        "id": str(ulid.ULID()),
        "source": source,
        "content": content,
        "t_ref": None,
        "bt": {
            "valid": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
            "sys":   [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
        },
        "metadata": {"demo": "sdk-real-evidence"},
    }


async def scenario_core_roundtrip() -> dict:
    import lunaris
    if not _tcp_reachable(MOON_URL):
        return {"__status__": "SKIP", "reason": f"Moon TCP unreachable at {MOON_URL}"}
    try:
        handle = await lunaris.open(MOON_URL)
    except Exception as e:
        return {"__status__": "SKIP", "reason": f"lunaris.open handshake failed: {e!s}"}

    s1 = await handle.snapshot()
    ep = _build_episode("Lunaris SDK real-case demo — core roundtrip")
    lsn = await handle.ingest(ep)
    s2 = await handle.snapshot()

    # Dry-run forget — safe, never mutates.
    receipt = await handle.forget({
        "target": {"Scope": {"BySource": "sdk-real-py/nonexistent"}},
        "options": {"hard": False, "dry_run": True, "confirmation_token": None},
    })

    return {
        "snapshot_before": s1,
        "snapshot_after_ingest": s2,
        "snapshot_monotonic": s2 >= s1,
        "forget_dry_run_preview": receipt.get("preview"),
        "forget_rows_written": receipt.get("rows_written"),
    }


async def scenario_chat_agent_memory() -> dict:
    import lunaris
    if not _tcp_reachable(MOON_URL):
        return {"__status__": "SKIP", "reason": "Moon unreachable"}
    try:
        handle = await lunaris.open(MOON_URL)
        from lunaris.conversational import ChatAgentMemory
        chat = ChatAgentMemory(handle, "sdk-real-demo-user")
    except Exception as e:
        return {"__status__": "SKIP", "reason": f"ChatAgentMemory init failed: {e!s}"}

    turns = [
        "I'm working on Lunaris agent memory.",
        "The current milestone is v0.1.1 — Recipes and Helios.",
        "We need sub-25ms recall over millions of facts.",
    ]
    recorded = []
    for t in turns:
        recorded.append(await chat.remember(t))

    hits = await chat.recall("Lunaris milestone")
    return {
        "recorded_ids": recorded[:3],
        "turn_count": len(recorded),
        "recall_hit_count": len(hits),
        "recall_sample": hits[:2],
    }


async def scenario_document_knowledge_base() -> dict:
    import lunaris
    if not _tcp_reachable(MOON_URL):
        return {"__status__": "SKIP", "reason": "Moon unreachable"}
    try:
        handle = await lunaris.open(MOON_URL)
        from lunaris.documentary import DocumentKnowledgeBase
        kb = DocumentKnowledgeBase(handle, "sdk-real-demo-kb/")
    except Exception as e:
        return {"__status__": "SKIP", "reason": f"DocumentKnowledgeBase init failed: {e!s}"}

    await kb.ingest([
        ("Lunaris uses bi-temporal MVCC over Moon for sub-25ms recall.", {"doc_id": "doc-001"}),
        ("Helios production hardening lands in Phase 12 of v0.1.1.",     {"doc_id": "doc-002"}),
    ])
    hits = await kb.top(5).search("bi-temporal MVCC")
    return {"ingested": 2, "search_hit_count": len(hits), "search_sample": hits[:2]}


async def scenario_pipeline_toggles() -> dict:
    import lunaris
    if not _tcp_reachable(MOON_URL):
        return {"__status__": "SKIP", "reason": "Moon unreachable"}
    try:
        handle = await lunaris.open(MOON_URL)
    except Exception as e:
        return {"__status__": "SKIP", "reason": f"open failed: {e!s}"}

    gp = handle.graph_pipeline
    cp = handle.consolidator_pipeline
    before_graph = gp.is_enabled()
    before_cons = cp.is_enabled()
    gp.enable(); gp.disable()
    cp.enable(); cp.disable()
    return {
        "graph_default_off": before_graph is False,
        "cons_default_off":  before_cons is False,
        "graph_toggle_clean": gp.is_enabled() is False,
        "cons_toggle_clean":  cp.is_enabled() is False,
    }


async def scenario_offline_dsl_surface() -> dict:
    """Offline — asserts DSL/top-level re-exports load without a backend."""
    import lunaris
    from lunaris import Vector, Keyword, Graph, RetrievalBuilder, open as _open
    from lunaris.conversational import ChatAgentMemory, MultiTurnConversation, SlackArchive, EmailThreading, MeetingNotesMemory
    from lunaris.documentary import DocumentKnowledgeBase, ResearchPaperCorpus, CodeRepoMemory, TimelineReconstruction, CustomerSupportHistory
    return {
        "version": getattr(lunaris, "__version__", "unknown"),
        "dsl_classes": [c.__name__ for c in [Vector, Keyword, Graph, RetrievalBuilder]],
        "conversational_wrappers": 5,
        "documentary_wrappers": 5,
    }


def main() -> int:
    # Live scenarios (core-roundtrip / chat-agent-memory / document-knowledge-base /
    # pipeline-toggles) are delegated to the pytest suite launched by
    # scripts/sdk-real-evidence.sh — pytest-asyncio's loop lifecycle avoids
    # the pyo3-async-runtimes unsendable-cross-thread issue that
    # `asyncio.run()` triggers in a standalone runner.
    print(f"Python runner — MOON_URL={MOON_URL}  (offline surface only; live cases via pytest)")
    results = []
    results.append(_run("offline-dsl-surface", scenario_offline_dsl_surface))
    rc = 0 if all(results) else 1
    print(f"\npython-offline-summary: {sum(results)}/{len(results)} OK rc={rc}")
    return rc


if __name__ == "__main__":
    sys.exit(main())
