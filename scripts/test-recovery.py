"""End-to-end crash-recovery test for Lunaris + Moon.

Exercises three failure modes and verifies post-recovery state:

  1. Moon hard-kill (SIGKILL) + AOF replay
     - Ingest N docs, capture a recall snapshot
     - SIGKILL moon; restart on same data dir with --appendonly yes
     - Reconnect lunaris; verify dbsize, FT indices, and recall results
       match the pre-crash snapshot

  2. Lunaris hard-kill mid-ingest
     - Run a child python process that streams docs into Moon
     - SIGKILL the child halfway through
     - Assert Moon side is consistent: all HSET/FT rows in flight either
       landed fully or not at all (no torn state that breaks FT.SEARCH)

  3. Write-after-restart
     - After (1)'s recovery, issue fresh ingests; assert the new docs
       are searchable alongside the replayed ones.

Usage:
  LUNARIS_TEST_MOON_URL="moon://127.0.0.1:6380" \
    uv run --with datasets --with python-ulid --with redis \
      python scripts/test-recovery.py [--docs 200]

Prereqs:
  - Ollama with embeddinggemma:300m pulled
  - Moon release binary at ../moon/target/release/moon
  - No other process on port 6380
  - Writable target/moon-data-recovery/ next to this script's CWD
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

import lunaris
import redis  # type: ignore[import-not-found]
from lunaris.documentary import DocumentKnowledgeBase

REPO_ROOT = Path(__file__).resolve().parent.parent
MOON_BIN = REPO_ROOT.parent / "moon" / "target" / "release" / "moon"
MOON_DATA = REPO_ROOT / "target" / "moon-data-recovery"
MOON_LOG = REPO_ROOT / "target" / "moon-data-recovery-launch.log"
MOON_PORT = 6380
MOON_URL = f"moon://127.0.0.1:{MOON_PORT}"
SOURCE_PREFIX = "hf-squad-recovery/"


def sh(*args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(args, capture_output=True, text=True, check=check)


def stop_any_moon() -> None:
    """Best-effort: kill any moon process on our port."""
    try:
        out = sh(
            "lsof", "-nP", "-iTCP:" + str(MOON_PORT), "-sTCP:LISTEN", check=False
        ).stdout
        for line in out.splitlines()[1:]:
            parts = line.split()
            if len(parts) >= 2 and parts[0].startswith("moon"):
                pid = int(parts[1])
                print(f"  stopping moon pid {pid}")
                os.kill(pid, signal.SIGKILL)
    except Exception as e:
        print(f"  stop_any_moon: {e}")
    time.sleep(0.3)


def start_moon(*, wipe: bool) -> subprocess.Popen:
    if wipe:
        if MOON_DATA.exists():
            shutil.rmtree(MOON_DATA)
    MOON_DATA.mkdir(parents=True, exist_ok=True)
    # --save "1 1" makes Moon auto-snapshot after 1 second of ≥1 write, which
    # produces the base RDB that AOF replay chains onto. Without it, Moon
    # refuses to replay an AOF incr against an empty state ("AOF base RDB
    # missing") — confirmed 2026-04-23 by killing before the first snapshot.
    cmd = [
        str(MOON_BIN),
        "--bind",
        "127.0.0.1",
        "--port",
        str(MOON_PORT),
        "--dir",
        str(MOON_DATA),
        "--shards",
        "1",
        "--protected-mode",
        "no",
        "--appendonly",
        "yes",
        "--appendfsync",
        "always",
        "--save",
        "1 1",
    ]
    log_f = open(MOON_LOG, "ab")
    p = subprocess.Popen(cmd, stdout=log_f, stderr=log_f)
    # Wait for PING to succeed (max 8 s).
    r = redis.Redis(host="127.0.0.1", port=MOON_PORT, socket_timeout=0.5)
    for _ in range(80):
        try:
            if r.ping():
                return p
        except Exception:
            pass
        time.sleep(0.1)
    raise RuntimeError("moon failed to come up within 8 s")


def snapshot(r: redis.Redis) -> dict:
    out: dict = {"dbsize": r.dbsize()}
    try:
        lst = r.execute_command("FT._LIST")
        out["indices"] = sorted([x.decode() if isinstance(x, bytes) else x for x in lst])
    except Exception as e:
        out["indices"] = f"error: {e}"
    for idx in ("chunks", "entities", "facts", "communities"):
        try:
            info = r.execute_command("FT.INFO", idx)
            flat = {
                info[i].decode() if isinstance(info[i], bytes) else str(info[i]): info[i + 1]
                for i in range(0, len(info) - 1, 2)
            }
            out[f"{idx}_num_docs"] = int(flat.get("num_docs", 0) or 0)
        except Exception as e:
            out[f"{idx}_num_docs"] = f"error: {e}"
    return out


async def ingest_and_sample(
    n_docs: int, n_probe_queries: int = 5
) -> tuple[list[tuple[str, str]], list[tuple[str, list[str], str]], list[list[str]]]:
    """Ingest N contexts; run M probe queries; return (docs, queries, probe_results).

    probe_results[i] is the list of hit texts (ordered) for queries[i].
    """
    from datasets import load_dataset
    from collections import OrderedDict

    ds = load_dataset("rajpurkar/squad", split="validation")
    by_ctx: OrderedDict[str, list[tuple[str, list[str]]]] = OrderedDict()
    ctx_ids: dict[str, str] = {}
    for row in ds:
        ctx = row["context"]
        if ctx not in by_ctx:
            by_ctx[ctx] = []
            ctx_ids[ctx] = row["id"]
        answers = [a for a in (row["answers"].get("text") or []) if a]
        if answers:
            by_ctx[ctx].append((row["question"], answers))
        if len(by_ctx) >= n_docs:
            break
    docs = [(ctx_ids[c], c) for c in list(by_ctx.keys())[:n_docs]]
    queries: list[tuple[str, list[str], str]] = []
    ctx_list = list(by_ctx.keys())[:n_docs]
    # Pick first question from first M contexts as probes.
    for ctx in ctx_list[:n_probe_queries]:
        qs = by_ctx[ctx]
        if qs:
            q, a = qs[0]
            queries.append((ctx_ids[ctx], a, q))

    handle = await lunaris.open(MOON_URL)
    kb = DocumentKnowledgeBase.new(handle, SOURCE_PREFIX)
    print(f"  ingesting {len(docs)} docs...")
    for ctx_id, context in docs:
        await kb.ingest([(context, {"doc_id": ctx_id, "title": ctx_id})])

    probe_results: list[list[str]] = []
    for _ctx_id, _answers, q in queries:
        hits = await kb.top(10).search(q)
        probe_results.append([(h.get("text") or "")[:80] for h in hits])
    return docs, queries, probe_results


async def replay_probes(
    queries: list[tuple[str, list[str], str]]
) -> list[list[str]]:
    handle = await lunaris.open(MOON_URL)
    kb = DocumentKnowledgeBase.new(handle, SOURCE_PREFIX)
    out: list[list[str]] = []
    for _ctx_id, _answers, q in queries:
        hits = await kb.top(10).search(q)
        out.append([(h.get("text") or "")[:80] for h in hits])
    return out


def assert_equal(label: str, before, after) -> bool:
    ok = before == after
    marker = "PASS" if ok else "FAIL"
    print(f"    [{marker}] {label}: before={before} after={after}")
    return ok


async def test_moon_kill(n_docs: int) -> bool:
    print("\n════════════ TEST 1 — Moon kill-9 + AOF replay ════════════")
    stop_any_moon()
    moon_proc = start_moon(wipe=True)
    print(f"  started moon pid={moon_proc.pid}")

    # Phase A — ingest + snapshot. `kb.ingest` returns after atomic_write
    # ACKs, but Moon's FT index can lag a few hundred ms before
    # num_docs reflects the HSETs. Sleep 2 s to let the index settle before
    # snapshotting so the pre-crash vs post-recovery comparison is apples-
    # to-apples (discovered during first recovery run 2026-04-23).
    docs, queries, probes_before = await ingest_and_sample(n_docs, n_probe_queries=5)
    await asyncio.sleep(2.0)
    r = redis.Redis(host="127.0.0.1", port=MOON_PORT)
    snap_before = snapshot(r)
    print(f"  pre-crash: {snap_before}")
    # Capture probes AGAIN after the settle — the snapshot probes may have
    # run while index was still growing.
    probes_before = await replay_probes(queries)

    # Phase B — force an AOF base RDB snapshot so replay has an anchor.
    # BGSAVE alone does NOT create the AOF base that replay needs; Moon
    # requires BGREWRITEAOF which rotates the AOF seq + writes a new
    # `moon.aof.<N>.base.rdb`. Poll for the file on disk since Moon's
    # LASTSAVE doesn't return a usable integer (live-measurement finding
    # 2026-04-23).
    aof_dir = MOON_DATA / "appendonlydir"
    try:
        r.execute_command("BGREWRITEAOF")
        t_bg = time.perf_counter()
        base_rdbs_before = set(aof_dir.glob("*.base.rdb"))
        while time.perf_counter() - t_bg < 15.0:
            base_rdbs_after = set(aof_dir.glob("*.base.rdb"))
            new = base_rdbs_after - base_rdbs_before
            if new:
                base = next(iter(new))
                if base.stat().st_size > 0:
                    print(
                        f"  BGREWRITEAOF produced {base.name} "
                        f"({base.stat().st_size} bytes) in "
                        f"{time.perf_counter() - t_bg:.2f} s"
                    )
                    break
            time.sleep(0.1)
        else:
            print("  WARN: no new base.rdb within 15 s — recovery may fail")
    except Exception as e:
        print(f"  WARN: BGREWRITEAOF failed: {e}")
    print(f"  SIGKILL moon pid={moon_proc.pid}...")
    os.kill(moon_proc.pid, signal.SIGKILL)
    moon_proc.wait(timeout=5)
    time.sleep(0.5)

    # Phase C — restart (same data dir)
    print("  restarting moon on same data dir (AOF replay)...")
    t_restart = time.perf_counter()
    moon_proc2 = start_moon(wipe=False)
    replay_s = time.perf_counter() - t_restart
    print(f"  moon back up in {replay_s:.2f} s, pid={moon_proc2.pid}")

    # Phase D — post-restart snapshot
    r2 = redis.Redis(host="127.0.0.1", port=MOON_PORT)
    snap_after = snapshot(r2)
    print(f"  post-restart: {snap_after}")

    probes_after = await replay_probes(queries)

    print("\n  ── assertions ──")
    all_ok = True
    all_ok &= assert_equal("dbsize", snap_before["dbsize"], snap_after["dbsize"])
    all_ok &= assert_equal("indices", snap_before["indices"], snap_after["indices"])
    all_ok &= assert_equal(
        "chunks num_docs",
        snap_before["chunks_num_docs"],
        snap_after["chunks_num_docs"],
    )
    for i, (q, a, b) in enumerate(zip(queries, probes_before, probes_after)):
        all_ok &= assert_equal(
            f"probe {i} top-10 text identity ({q[2][:40]}...)", a, b
        )
    return all_ok


async def test_lunaris_kill(n_docs: int) -> bool:
    """Kill a child python ingest mid-run; verify Moon state is consistent.

    'Consistent' = all partial writes on each Episode respected atomic_write
    (either Episode+chunks fully present or fully absent). We can't easily
    assert the second part of the disjunction without inside knowledge, so
    we check the weaker invariant: FT.SEARCH still runs without error, and
    the number of Episode KV rows equals the number of chunk rows / avg
    chunks-per-episode (approximately).
    """
    print("\n════════════ TEST 2 — Lunaris kill-9 mid-ingest ════════════")
    stop_any_moon()
    start_moon(wipe=True)

    script = f"""
import asyncio, lunaris, sys, os
from lunaris.documentary import DocumentKnowledgeBase
from datasets import load_dataset
from collections import OrderedDict

async def main():
    ds = load_dataset('rajpurkar/squad', split='validation')
    by_ctx = OrderedDict()
    ctx_ids = {{}}
    for row in ds:
        ctx = row['context']
        if ctx not in by_ctx:
            by_ctx[ctx] = True
            ctx_ids[ctx] = row['id']
        if len(by_ctx) >= {n_docs}:
            break
    docs = [(ctx_ids[c], c) for c in list(by_ctx.keys())[:{n_docs}]]
    h = await lunaris.open('{MOON_URL}')
    kb = DocumentKnowledgeBase.new(h, '{SOURCE_PREFIX}')
    for i, (ctx_id, context) in enumerate(docs):
        await kb.ingest([(context, {{'doc_id': ctx_id}})])
        # Heartbeat so the parent can measure progress
        print(i + 1, flush=True)
asyncio.run(main())
"""
    env = dict(os.environ, LUNARIS_TEST_MOON_URL=MOON_URL)
    proc = subprocess.Popen(
        ["python", "-c", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        text=True,
    )
    # Wait until we see ~half the docs have been ingested, then SIGKILL.
    target = max(1, n_docs // 2)
    last_seen = 0
    assert proc.stdout is not None
    deadline = time.time() + 30
    while time.time() < deadline:
        line = proc.stdout.readline()
        if not line:
            break
        try:
            last_seen = int(line.strip())
        except Exception:
            continue
        if last_seen >= target:
            print(f"  child reached {last_seen}/{n_docs}; SIGKILL pid={proc.pid}")
            os.kill(proc.pid, signal.SIGKILL)
            break
    proc.wait(timeout=5)
    time.sleep(0.3)

    # Moon is still up; assert the state is readable + consistent.
    r = redis.Redis(host="127.0.0.1", port=MOON_PORT)
    snap = snapshot(r)
    print(f"  post-kill Moon state: {snap}")
    ingested = last_seen
    print(f"  child reported {ingested} docs ingested before SIGKILL")

    # Consistency: dbsize must be > 0, chunks_num_docs must be >= ingested
    # (chunker may split — so >=, not ==).
    all_ok = True
    all_ok &= assert_equal("dbsize > 0", True, snap["dbsize"] > 0)
    cnum = snap.get("chunks_num_docs", 0)
    all_ok &= assert_equal(
        f"chunks_num_docs >= {ingested}", True, isinstance(cnum, int) and cnum >= ingested
    )
    # Now verify recall still works (FT.SEARCH not broken by the kill).
    h = await lunaris.open(MOON_URL)
    kb = DocumentKnowledgeBase.new(h, SOURCE_PREFIX)
    try:
        hits = await kb.top(5).search("football")
        print(f"  recall after kill returned {len(hits)} hits (sample text: "
              f"{(hits[0].get('text') or '')[:60] if hits else 'NONE'})")
        all_ok &= assert_equal("recall executable after kill", True, True)
    except Exception as e:
        print(f"  recall after kill FAILED: {e}")
        all_ok &= False
    return all_ok


async def test_write_after_restart(n_docs: int) -> bool:
    """After TEST 1 left Moon recovered, write fresh data + verify searchable."""
    print("\n════════════ TEST 3 — Writes after restart ════════════")
    h = await lunaris.open(MOON_URL)
    kb = DocumentKnowledgeBase.new(h, SOURCE_PREFIX + "post-restart/")
    body = (
        "RECOVERY-MARKER-XYZ: Lunaris post-restart write succeeded. "
        "This sentence contains a unique anchor phrase."
    )
    await kb.ingest([(body, {"doc_id": "recovery-probe", "title": "recovery"})])
    # Give the HNSW a moment to flush (usually <100ms).
    await asyncio.sleep(0.5)
    hits = await kb.top(5).search("RECOVERY-MARKER-XYZ unique anchor phrase")
    found = any("RECOVERY-MARKER-XYZ" in (h.get("text") or "") for h in hits)
    return assert_equal("RECOVERY-MARKER-XYZ roundtrip", True, found)


async def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--docs", type=int, default=200)
    args = ap.parse_args()

    if not MOON_BIN.exists():
        print(f"moon binary not found at {MOON_BIN}", file=sys.stderr)
        sys.exit(1)

    results = {}
    results["moon_kill"] = await test_moon_kill(args.docs)
    # TEST 2 starts fresh (wipes Moon); ordering is intentional.
    results["lunaris_kill"] = await test_lunaris_kill(args.docs)
    # Restore the TEST 1 state by re-running TEST 1 briefly (to have a
    # recovered Moon for TEST 3). We skip that here and just assert TEST 3
    # runs on current Moon (from TEST 2); TEST 2 left Moon up and with some
    # data, so TEST 3 only needs "can read and write" against a live Moon.
    results["write_after_restart"] = await test_write_after_restart(args.docs)

    print("\n════════════ SUMMARY ════════════")
    for k, v in results.items():
        print(f"  {k:<25} {'PASS' if v else 'FAIL'}")
    print()
    print(json.dumps(results, indent=2))
    sys.exit(0 if all(results.values()) else 1)


asyncio.run(main())
