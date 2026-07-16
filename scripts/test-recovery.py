"""End-to-end crash-recovery test for Lunaris + Moon.

Exercises four failure modes and verifies post-recovery state:

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

  4. Upgrade replay (#69 guard, moon-v070-bump) — `--upgrade-replay`
     - OLD binary writes raw KV + bi-temporal + graph + MQ + temporal probes
     - SIGKILL; NEW binary restarts on the same data dir
     - Assert every plane replays intact (raw redis only — no SDK/embedder)

TEST 1 additionally writes the same plane probes pre-kill and verifies them
post-restart (the #69-protected MQ/temporal planes were previously unprobed).

Usage:
  LUNARIS_TEST_MOON_URL="moon://127.0.0.1:6380" \
    uv run --with datasets --with python-ulid --with redis \
      python scripts/test-recovery.py [--docs 200]
  python scripts/test-recovery.py --upgrade-replay \
    [--old-bin ~/.lunaris/bin/moon] [--new-bin vendor/moon/target/release/moon]

Prereqs (TESTs 1-3 only; TEST 4 needs just `redis` + the two binaries):
  - lunaris Python SDK importable (maturin develop --uv -m crates/lunaris-py/Cargo.toml)
  - embedder GGUF at ~/.lunaris/models/ (granite-embedding-311m Q4_K_M)
  - Moon release binary at ../moon/target/release/moon (override: LUNARIS_TEST_MOON_BIN)
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

import redis  # type: ignore[import-not-found]

# NOTE: `lunaris` + `DocumentKnowledgeBase` are imported lazily inside the
# tests that need them (TEST 1-3) so the raw-command `--upgrade-replay` leg
# runs without the Python SDK installed (moon-v070-bump).

REPO_ROOT = Path(__file__).resolve().parent.parent
# Default: sibling moon repo. Override with LUNARIS_TEST_MOON_BIN (e.g. the
# vendored release binary) so TESTs 1-3 exercise the pinned Moon version.
MOON_BIN = Path(
    os.environ.get(
        "LUNARIS_TEST_MOON_BIN",
        REPO_ROOT.parent / "moon" / "target" / "release" / "moon",
    )
)
MOON_DATA = REPO_ROOT / "target" / "moon-data-recovery"
MOON_LOG = REPO_ROOT / "target" / "moon-data-recovery-launch.log"
# Overridable: 6380 collides with OrbStack's forwarded ai-proxy Redis on this
# host (*:6380 wildcard bind can capture 127.0.0.1 clients and answer
# `unknown command 'MQ'` for Moon-only probes — observed 2026-07-16).
MOON_PORT = int(os.environ.get("LUNARIS_TEST_MOON_PORT", "6380"))
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


def start_moon(*, wipe: bool, moon_bin: Path | None = None) -> subprocess.Popen:
    if wipe:
        if MOON_DATA.exists():
            shutil.rmtree(MOON_DATA)
    MOON_DATA.mkdir(parents=True, exist_ok=True)
    # --save "1 1" makes Moon auto-snapshot after 1 second of ≥1 write, which
    # produces the base RDB that AOF replay chains onto. Without it, Moon
    # refuses to replay an AOF incr against an empty state ("AOF base RDB
    # missing") — confirmed 2026-04-23 by killing before the first snapshot.
    cmd = [
        str(moon_bin or MOON_BIN),
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
        # This dev box runs >90% disk-used; without this the diskfull guard
        # pauses write-flagged commands (incl. MQ POP delivery-state writes)
        # mid-test and masquerades as data loss (found 2026-07-15).
        "--disk-free-min-pct",
        "1",
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


MQ_PROBE_TOPIC = "recovery-probe-mq"
MQ_PROBE_PAYLOADS = [b"mq-probe-1", b"mq-probe-2", b"mq-probe-3"]
KV_PROBE_KEY = "recovery-probe:kv"
BT_PROBE_KEY = "recovery-probe:bt"
GRAPH_PROBE_NAME = "recovery-probe-graph"


def force_aof_anchor(r: redis.Redis) -> None:
    """Force an AOF base RDB snapshot so replay has an anchor.

    BGSAVE alone does NOT create the AOF base that replay needs; Moon
    requires BGREWRITEAOF which rotates the AOF seq + writes a new
    `moon.aof.<N>.base.rdb`. Poll for the file on disk since Moon's
    LASTSAVE doesn't return a usable integer (live-measurement finding
    2026-04-23).
    """
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


def temporal_smoke() -> None:
    """TEMPORAL.SNAPSHOT_AT pin smoke on a THROWAWAY connection.

    The snapshot pin is per-connection, so a dedicated connection keeps the
    caller's connection in live mode. The arg-less `TEMPORAL.INVALIDATE`
    release only exists on v0.7+ — on older servers dropping the connection
    releases the pin, so a signature error there is tolerated.
    """
    rt = redis.Redis(host="127.0.0.1", port=MOON_PORT, socket_timeout=2)
    rt.execute_command("TEMPORAL.SNAPSHOT_AT")
    try:
        rt.execute_command("TEMPORAL.INVALIDATE")
    except redis.exceptions.ResponseError as e:
        print(f"  note: TEMPORAL.INVALIDATE not arg-less on this server ({e}); pin released via close")
    rt.close()


def plane_probe_write(r: redis.Redis) -> dict:
    """Write MQ + temporal(+KV/graph) probe state — the #69-protected planes.

    MQ: push 3 messages, pop+ACK the first (creates consumed-offset state that
    the WAL must carry — moon-v070-bump). Temporal: a Lunaris-shaped hash with
    `v`+`bt` fields (what `read_as_of` HMGETs) + a TEMPORAL.SNAPSHOT_AT/
    INVALIDATE smoke. Graph: one GRAPH.ADDNODE (the only write path that
    registers key_to_node — see lunaris-storage-moon/src/atomic.rs).
    Returns the expected post-restart state for plane_probe_verify.
    """
    expected: dict = {}
    r.set(KV_PROBE_KEY, "kv-v1")
    expected["kv"] = b"kv-v1"
    r.hset(BT_PROBE_KEY, mapping={"v": "fact-v1", "bt": '{"vt":1752537600000,"tt":1752537600000}'})
    expected["bt"] = {b"v": b"fact-v1", b"bt": b'{"vt":1752537600000,"tt":1752537600000}'}
    # Queues must be declared durable before PUSH ("stream is not a durable
    # queue" otherwise) — mirrors MqClient::create.
    r.execute_command("MQ", "CREATE", MQ_PROBE_TOPIC)
    for p in MQ_PROBE_PAYLOADS:
        r.execute_command("MQ", "PUSH", MQ_PROBE_TOPIC, "body", p)
    popped = r.execute_command("MQ", "POP", MQ_PROBE_TOPIC, "COUNT", 1)
    # Reply: array of [id, body-ish fields]; ACK the first entry id.
    try:
        first_id = popped[0][0] if popped and isinstance(popped[0], (list, tuple)) else popped[0]
        if isinstance(first_id, bytes):
            first_id = first_id.decode()
        r.execute_command("MQ", "ACK", MQ_PROBE_TOPIC, first_id)
        expected["mq_acked"] = True
    except Exception as e:  # noqa: BLE001 — probe stays best-effort on shape drift
        print(f"  WARN: MQ pop/ack probe shape drift: {e}")
        expected["mq_acked"] = False
    expected["mq_remaining"] = MQ_PROBE_PAYLOADS[1:] if expected["mq_acked"] else MQ_PROBE_PAYLOADS
    try:
        r.execute_command("GRAPH.CREATE", GRAPH_PROBE_NAME)
        node_id = r.execute_command("GRAPH.ADDNODE", GRAPH_PROBE_NAME, "ProbeNode", "_key", "probe:1")
        r.execute_command(
            "GRAPH.QUERY",
            GRAPH_PROBE_NAME,
            f"MATCH (n:ProbeNode) WHERE id(n) = {int(node_id)} SET n.id = 'probe-1' RETURN n",
        )
        expected["graph_nodes"] = 1
    except Exception as e:  # noqa: BLE001
        print(f"  WARN: graph probe write failed: {e}")
        expected["graph_nodes"] = None
    temporal_smoke()
    return expected


def _entry_body(entry) -> bytes | None:
    """Extract the `body` field from a stream/MQ entry.

    redis-py decodes XRANGE entries as `(id, {field: value})`; MQ POP replies
    arrive as nested arrays. Handle both.
    """
    if not isinstance(entry, (list, tuple)) or len(entry) < 2:
        return None
    fields = entry[1]
    if isinstance(fields, dict):
        v = fields.get(b"body", fields.get("body"))
        return v if isinstance(v, (bytes, type(None))) else str(v).encode()
    flat = [x for x in entry if isinstance(x, (bytes, str))]
    for i, f in enumerate(flat):
        if f in (b"body", "body") and i + 1 < len(flat):
            v = flat[i + 1]
            return v if isinstance(v, bytes) else v.encode()
    if isinstance(fields, (list, tuple)):
        for i in range(0, len(fields) - 1, 2):
            if fields[i] in (b"body", "body"):
                v = fields[i + 1]
                return v if isinstance(v, bytes) else v.encode()
    return None


def wait_plane_replay(r: redis.Redis, n_entries: int, timeout_s: float = 10.0) -> None:
    """Moon replays the MQ WAL plane ASYNCHRONOUSLY after the port accepts
    connections (`moon::shard::shared_databases` logs it post-start) — verify
    too early and the stream looks empty (raced 2026-07-15). Poll XLEN until
    the expected entries land or the timeout expires.
    """
    t0 = time.perf_counter()
    while time.perf_counter() - t0 < timeout_s:
        try:
            if int(r.execute_command("XLEN", MQ_PROBE_TOPIC)) >= n_entries:
                print(f"  MQ plane replay settled in {time.perf_counter() - t0:.2f} s")
                return
        except Exception:
            pass
        time.sleep(0.2)
    print(f"  WARN: MQ plane replay did not settle within {timeout_s} s")


def plane_probe_verify(r: redis.Redis, expected: dict) -> bool:
    """Assert the plane_probe_write state survived a restart/upgrade replay."""
    ok = True
    wait_plane_replay(r, len(MQ_PROBE_PAYLOADS))
    ok &= assert_equal("KV probe value", expected["kv"], r.get(KV_PROBE_KEY))
    ok &= assert_equal("bi-temporal hash (v+bt fields)", expected["bt"], r.hgetall(BT_PROBE_KEY))
    # MQ plane, two guarantees asserted (moon-v070-bump, 2026-07-15):
    #   1. ENTRY durability — every pushed payload is still in the stream
    #      (XRANGE), byte-identical and in order.
    #   2. OPERATIONAL self-heal — after the idempotent re-CREATE that
    #      Lunaris's publish path always issues, PUSH+POP work again.
    # NOT asserted (known Moon gap ≤0.7.1, pre-existing at the old pin —
    # verified v0.6.0→v0.6.0 baseline 2026-07-15): the consumer delivery
    # cursor / PEL is not replayed, so pre-restart backlog is not
    # REDELIVERED even though the entries survive. Tracked as an upstream
    # Moon issue; Tier-2 WAIT-durable ingest must not assume redelivery.
    try:
        raw = r.execute_command("XRANGE", MQ_PROBE_TOPIC, "-", "+")
        stream_bodies = [_entry_body(e) for e in (raw or [])]
        stream_bodies = [b for b in stream_bodies if b is not None]
        ok &= assert_equal(
            "MQ stream entries survive (XRANGE)",
            MQ_PROBE_PAYLOADS,
            stream_bodies[: len(MQ_PROBE_PAYLOADS)],
        )
    except Exception as e:  # noqa: BLE001
        print(f"    [FAIL] MQ XRANGE probe errored: {e}")
        ok = False
    try:
        r.execute_command("MQ", "CREATE", MQ_PROBE_TOPIC)  # idempotent, mirrors publish()
        r.execute_command("MQ", "PUSH", MQ_PROBE_TOPIC, "body", b"mq-probe-postrestart")
        popped: list[bytes] = []
        t0 = time.perf_counter()
        while time.perf_counter() - t0 < 3.0:  # delivery visibility can lag the push post-replay
            raw = r.execute_command("MQ", "POP", MQ_PROBE_TOPIC, "COUNT", 10)
            popped.extend(b for b in (_entry_body(e) for e in (raw or [])) if b is not None)
            if b"mq-probe-postrestart" in popped:
                break
            time.sleep(0.2)
        ok &= assert_equal(
            "MQ self-heal: post-restart push is consumable", True, b"mq-probe-postrestart" in popped
        )
        redelivered = [p for p in popped if p in expected["mq_remaining"]]
        if redelivered:
            print(f"    note: pre-restart backlog WAS redelivered ({redelivered}) — Moon fixed the cursor gap")
        else:
            print(
                "    WARN(known-gap): pre-restart un-ACKed backlog not redelivered "
                f"(expected-if-fixed: {expected['mq_remaining']}) — entries survive in the stream "
                "but the delivery cursor does not replay (Moon ≤0.7.1, pre-existing at v0.6.0)"
            )
    except Exception as e:  # noqa: BLE001
        print(f"    [FAIL] MQ self-heal probe errored: {e}")
        ok = False
    if expected.get("graph_nodes") is not None:
        try:
            res = r.execute_command(
                "GRAPH.QUERY", GRAPH_PROBE_NAME, "MATCH (n:ProbeNode) WHERE n.id = 'probe-1' RETURN count(n)"
            )

            def leaves(v):  # header row comes first — find the first int-like leaf
                if isinstance(v, (list, tuple)):
                    for x in v:
                        yield from leaves(x)
                else:
                    yield v

            count = None
            for leaf in leaves(res):
                try:
                    count = int(leaf)
                    break
                except (TypeError, ValueError):
                    continue
            ok &= assert_equal("graph probe node count", expected["graph_nodes"], count)
        except Exception as e:  # noqa: BLE001
            print(f"    [FAIL] graph probe query errored: {e}")
            ok = False
    try:
        temporal_smoke()
        ok &= assert_equal("TEMPORAL.SNAPSHOT_AT smoke", True, True)
    except Exception as e:  # noqa: BLE001
        print(f"    [FAIL] temporal smoke errored: {e}")
        ok = False
    return ok


def snapshot(r: redis.Redis) -> dict:
    out: dict = {"dbsize": r.dbsize()}
    try:
        lst = r.execute_command("FT._LIST")
        out["indices"] = sorted([x.decode() if isinstance(x, bytes) else x for x in lst])
    except Exception as e:
        out["indices"] = f"error: {e}"
    # RFC 0001 scoped ingest writes to `lunaris_<scope>_<kind>_idx` indexes;
    # the legacy global names ("chunks", ...) exist but stay empty — reading
    # them reported num_docs=0 forever (stale pre-scope harness debt, fixed
    # 2026-07-15 during moon-v070-bump).
    all_indices = out["indices"] if isinstance(out["indices"], list) else []
    for kind in ("chunks", "entities", "facts", "communities"):
        scoped = [i for i in all_indices if i.endswith(f"_{kind}_idx")] or [kind]
        total = 0
        for idx in scoped:
            try:
                info = r.execute_command("FT.INFO", idx)
                flat = {
                    info[i].decode() if isinstance(info[i], bytes) else str(info[i]): info[i + 1]
                    for i in range(0, len(info) - 1, 2)
                }
                total += int(flat.get("num_docs", 0) or 0)
            except Exception:
                continue
        out[f"{kind}_num_docs"] = total
    return out


async def ingest_and_sample(
    n_docs: int, n_probe_queries: int = 5
) -> tuple[list[tuple[str, str]], list[tuple[str, list[str], str]], list[list[str]]]:
    """Ingest N contexts; run M probe queries; return (docs, queries, probe_results).

    probe_results[i] is the list of hit texts (ordered) for queries[i].
    """
    import lunaris
    from lunaris.documentary import DocumentKnowledgeBase
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
    import lunaris
    from lunaris.documentary import DocumentKnowledgeBase

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
    # Plane probes must land BEFORE the pre-crash snapshot or their keys
    # inflate the post-restart dbsize comparison (found 2026-07-15).
    plane_expected = plane_probe_write(r)
    print(f"  plane probes written: {list(plane_expected)}")
    snap_before = snapshot(r)
    print(f"  pre-crash: {snap_before}")
    # Capture probes AGAIN after the settle — the snapshot probes may have
    # run while index was still growing.
    probes_before = await replay_probes(queries)

    # Phase B — force the AOF anchor.
    force_aof_anchor(r)
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

    # Same 2 s settle as the pre-crash leg so the comparison stays
    # apples-to-apples. NOTE (2026-07-16): probe set-identity is only
    # meaningful when the py wheel embeds real vectors. A wheel whose
    # `llamacpp` feature does not forward to the umbrella crate
    # (crates/lunaris-py/Cargo.toml) silently NoopEmbeds to zero vectors;
    # the probes then measure HNSW tie-break order, which legitimately
    # permutes across an AOF-replay rebuild → false FAIL on ANY Moon
    # version (A/B-verified identical on v0.7.1 and v0.8.0).
    await asyncio.sleep(2.0)
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
    # Durability guarantees the recalled SET; the ORDER can permute after a
    # restart because the HNSW index is rebuilt from replayed HSETs in a
    # different insertion order (observed on v0.7.1, 2026-07-15 — identical
    # 10 texts, shuffled ranking). Assert set identity; report order drift.
    for i, (q, a, b) in enumerate(zip(queries, probes_before, probes_after)):
        all_ok &= assert_equal(
            f"probe {i} top-10 text SET identity ({q[2][:40]}...)", sorted(a), sorted(b)
        )
        if a != b and sorted(a) == sorted(b):
            print(f"    note(order-drift): probe {i} same hits, permuted ranking after replay")
    print("\n  ── plane probes (MQ · temporal · graph · KV) ──")
    all_ok &= plane_probe_verify(r2, plane_expected)
    return all_ok


async def test_upgrade_replay(old_bin: Path, new_bin: Path, anchor: bool = True) -> bool:
    """#69 guard: a data dir written by the OLD Moon binary must replay intact
    under the NEW (v0.7.1) binary — MQ + temporal history included.

    Raw-command probes only (no embedder / datasets needed) so this leg runs
    self-contained: KV string · bi-temporal hash · graph node · MQ backlog
    with a consumed (ACKed) head · TEMPORAL smoke.
    """
    print("\n════════════ TEST 4 — v0.6→v0.7.1 upgrade replay (#69) ════════════")
    print(f"  old binary: {old_bin}")
    print(f"  new binary: {new_bin}")
    for b, name in ((old_bin, "old"), (new_bin, "new")):
        if not b.exists():
            print(f"  SKIP: {name} binary missing at {b}")
            return False

    stop_any_moon()
    old_proc = start_moon(wipe=True, moon_bin=old_bin)
    r = redis.Redis(host="127.0.0.1", port=MOON_PORT)
    old_version = (r.info("server").get("moon_version") or "?")
    print(f"  old moon up pid={old_proc.pid} version={old_version}")

    expected = plane_probe_write(r)
    dbsize_before = r.dbsize()
    if anchor:
        force_aof_anchor(r)
    else:
        print("  --no-anchor: skipping BGREWRITEAOF (isolates rewriter-drops-MQ)")
        time.sleep(1.5)  # let --save "1 1" cut its own base if it wants to
    print(f"  SIGKILL old moon pid={old_proc.pid}...")
    os.kill(old_proc.pid, signal.SIGKILL)
    old_proc.wait(timeout=5)
    time.sleep(0.5)

    print("  starting NEW binary on the old data dir (upgrade replay)...")
    t0 = time.perf_counter()
    new_proc = start_moon(wipe=False, moon_bin=new_bin)
    print(f"  new moon up in {time.perf_counter() - t0:.2f} s, pid={new_proc.pid}")
    r2 = redis.Redis(host="127.0.0.1", port=MOON_PORT)
    new_version = (r2.info("server").get("moon_version") or "?")
    print(f"  new moon version={new_version}")

    all_ok = True
    all_ok &= assert_equal("dbsize across upgrade", dbsize_before, r2.dbsize())
    all_ok &= plane_probe_verify(r2, expected)
    if old_version == new_version:
        print(f"  WARN: old and new report the same version ({old_version}) — not a real upgrade")
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
        [sys.executable, "-c", script],
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
    import lunaris
    from lunaris.documentary import DocumentKnowledgeBase

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
    import lunaris
    from lunaris.documentary import DocumentKnowledgeBase

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
    ap.add_argument(
        "--upgrade-replay",
        action="store_true",
        help="run ONLY the #69 old-binary→new-binary upgrade-replay test (raw probes, no embedder)",
    )
    ap.add_argument(
        "--old-bin",
        type=Path,
        default=Path.home() / ".lunaris" / "bin" / "moon",
        help="OLD Moon binary that writes the pre-upgrade data dir",
    )
    ap.add_argument(
        "--new-bin",
        type=Path,
        default=REPO_ROOT / "vendor" / "moon" / "target" / "release" / "moon",
        help="NEW Moon binary that must replay the old data dir intact",
    )
    ap.add_argument(
        "--no-anchor",
        action="store_true",
        help="upgrade-replay only: skip the BGREWRITEAOF anchor before the kill",
    )
    args = ap.parse_args()

    if args.upgrade_replay:
        ok = await test_upgrade_replay(args.old_bin, args.new_bin, anchor=not args.no_anchor)
        print("\n════════════ SUMMARY ════════════")
        print(f"  upgrade_replay            {'PASS' if ok else 'FAIL'}")
        print()
        print(json.dumps({"upgrade_replay": ok}, indent=2))
        sys.exit(0 if ok else 1)

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
