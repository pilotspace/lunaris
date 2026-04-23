"""Larger live-Moon benchmark over SQuAD paragraphs with embeddinggemma:300m.

Loads N unique contexts from rajpurkar/squad, ingests them through
DocumentKnowledgeBase, then issues M queries whose gold paragraph is known,
measuring:

  Ingest
    total wall  |  docs/sec  |  per-doc p50/p95/p99 latency

  Recall
    per-query p50/p95/p99 latency
    recall@1 / @3 / @5 / @10
    MRR (mean reciprocal rank of the gold doc)

  Moon footprint
    dbsize, per-index num_docs, used_memory

  Resource usage (when psutil is available)
    per-process CPU% / RSS peak + mean for moon, ollama, and this bench
    system CPU%, used memory, load average — sampled once per second

Usage:
  LUNARIS_TEST_MOON_URL="moon://127.0.0.1:6380" \
    uv run --with datasets --with python-ulid --with redis --with psutil \
      python scripts/bench-squad-kb.py \
        [--docs 300] [--queries 100] [--top-k 10] [--split validation|train]
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import statistics
import threading
import time
from collections import OrderedDict

import lunaris
from lunaris.documentary import DocumentKnowledgeBase

MOON_URL = os.environ.get("LUNARIS_TEST_MOON_URL", "moon://127.0.0.1:6380")
SOURCE_PREFIX = "hf-squad/"


def pct(xs: list[float], p: float) -> float:
    if not xs:
        return float("nan")
    xs = sorted(xs)
    k = max(0, min(len(xs) - 1, int(round((p / 100.0) * (len(xs) - 1)))))
    return xs[k]


# ──────────────────────────────────────────────────────────────────────────
# Resource sampler — optional (no-op when psutil isn't installed)
# ──────────────────────────────────────────────────────────────────────────


class ResourceSampler:
    """Sample CPU% + RSS for named processes + the overall system once/sec."""

    def __init__(self, targets: list[str], interval_s: float = 1.0) -> None:
        self.targets = targets  # process name substrings to track
        self.interval_s = interval_s
        self.samples: dict[str, list[tuple[float, float]]] = {
            t: [] for t in targets
        }
        self.system_cpu: list[float] = []
        self.system_mem_pct: list[float] = []
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._psutil = None
        try:
            import psutil  # type: ignore[import-not-found]

            self._psutil = psutil
            # Prime cpu_percent so the next call returns a real delta.
            psutil.cpu_percent(interval=None)
        except ImportError:
            pass

    def start(self) -> None:
        if self._psutil is None:
            return
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def _find_procs(self) -> dict[str, list]:
        assert self._psutil is not None
        procs: dict[str, list] = {t: [] for t in self.targets}
        for p in self._psutil.process_iter(attrs=["pid", "name", "cmdline"]):
            try:
                name = (p.info.get("name") or "").lower()
                cmd = " ".join(p.info.get("cmdline") or []).lower()
                for t in self.targets:
                    needle = t.lower()
                    if needle in name or needle in cmd:
                        procs[t].append(p)
                        # Prime per-proc cpu_percent for the next call.
                        try:
                            p.cpu_percent(interval=None)
                        except Exception:
                            pass
            except Exception:
                continue
        return procs

    def _run(self) -> None:
        assert self._psutil is not None
        procs = self._find_procs()
        # Give cpu_percent one interval to warm up.
        time.sleep(self.interval_s)
        while not self._stop.is_set():
            try:
                self.system_cpu.append(self._psutil.cpu_percent(interval=None))
                self.system_mem_pct.append(self._psutil.virtual_memory().percent)
                for t, plist in procs.items():
                    cpu_sum = 0.0
                    rss_sum = 0.0
                    alive = []
                    for p in plist:
                        try:
                            cpu_sum += p.cpu_percent(interval=None)
                            rss_sum += p.memory_info().rss / (1024 * 1024)
                            alive.append(p)
                        except Exception:
                            continue
                    procs[t] = alive
                    if alive:
                        self.samples[t].append((cpu_sum, rss_sum))
                    # Refresh process list periodically so newly-spawned
                    # ollama children get picked up.
                    if len(self.system_cpu) % 10 == 0:
                        for new_p in self._psutil.process_iter(attrs=["name", "cmdline"]):
                            try:
                                name = (new_p.info.get("name") or "").lower()
                                cmd = " ".join(new_p.info.get("cmdline") or []).lower()
                                needle = t.lower()
                                if (
                                    (needle in name or needle in cmd)
                                    and new_p not in plist
                                ):
                                    plist.append(new_p)
                                    try:
                                        new_p.cpu_percent(interval=None)
                                    except Exception:
                                        pass
                            except Exception:
                                continue
            except Exception:
                pass
            self._stop.wait(self.interval_s)

    def stop(self) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=2.0)

    def report(self) -> dict[str, object]:
        if self._psutil is None:
            return {"note": "psutil not installed — resource monitoring skipped"}
        out: dict[str, object] = {"samples": len(self.system_cpu)}
        for t, pts in self.samples.items():
            if not pts:
                out[t] = {"note": "no process matched"}
                continue
            cpus = [x[0] for x in pts]
            rss = [x[1] for x in pts]
            out[t] = {
                "cpu_pct_mean": round(statistics.mean(cpus), 1),
                "cpu_pct_p95": round(pct(cpus, 95), 1),
                "cpu_pct_max": round(max(cpus), 1),
                "rss_mb_mean": round(statistics.mean(rss), 1),
                "rss_mb_peak": round(max(rss), 1),
            }
        if self.system_cpu:
            out["system"] = {
                "cpu_pct_mean": round(statistics.mean(self.system_cpu), 1),
                "cpu_pct_p95": round(pct(self.system_cpu, 95), 1),
                "cpu_pct_max": round(max(self.system_cpu), 1),
                "mem_pct_mean": round(statistics.mean(self.system_mem_pct), 1),
                "mem_pct_peak": round(max(self.system_mem_pct), 1),
            }
        return out


def load_squad(
    split: str, n_docs: int, n_queries: int
) -> tuple[list[tuple[str, str]], list[tuple[str, list[str], str]]]:
    """Return (docs, queries).

    docs    = [(ctx_id, context_text)]
    queries = [(ctx_id, answer_spans, question_text)]
    """
    from datasets import load_dataset

    ds = load_dataset("rajpurkar/squad", split=split)
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
        if len(by_ctx) >= n_docs and all(
            len(by_ctx[k]) >= 1 for k in list(by_ctx)[:n_docs]
        ):
            break
    docs = [(ctx_ids[c], c) for c in list(by_ctx.keys())[:n_docs]]
    queries: list[tuple[str, list[str], str]] = []
    ctx_list = list(by_ctx.keys())[:n_docs]
    cursor = [list(by_ctx[c]) for c in ctx_list]
    i = 0
    while len(queries) < n_queries and any(cursor):
        idx = i % len(cursor)
        bucket = cursor[idx]
        if bucket:
            q, answers = bucket.pop()
            queries.append((ctx_ids[ctx_list[idx]], answers, q))
        i += 1
    return docs, queries


async def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--docs", type=int, default=300)
    ap.add_argument("--queries", type=int, default=100)
    ap.add_argument("--top-k", type=int, default=10)
    ap.add_argument("--split", default="validation", choices=["validation", "train"])
    ap.add_argument(
        "--progress-every",
        type=int,
        default=0,
        help="Print ingest progress every N docs (0 = off)",
    )
    args = ap.parse_args()

    print(f"# Backend      : {MOON_URL}")
    print(f"# Embedder     : Ollama embeddinggemma:300m (768d Google EmbeddingGemma)")
    print(f"# Corpus       : rajpurkar/squad {args.split}")
    print(
        f"# Plan         : ingest {args.docs} paragraphs, query {args.queries} times, "
        f"top-{args.top_k}"
    )

    docs, queries = load_squad(args.split, args.docs, args.queries)
    print(f"# Loaded       : {len(docs)} unique contexts, {len(queries)} queries")

    sampler = ResourceSampler(targets=["moon", "ollama", "bench-squad-kb"])
    sampler.start()

    handle = await lunaris.open(MOON_URL)
    kb = DocumentKnowledgeBase.new(handle, SOURCE_PREFIX)

    # ── Ingest ────────────────────────────────────────────────────────────
    ingest_latencies: list[float] = []
    ingest_start = time.perf_counter()
    for i, (ctx_id, context) in enumerate(docs, start=1):
        meta = {"doc_id": ctx_id, "title": ctx_id}
        t = time.perf_counter()
        await kb.ingest([(context, meta)])
        ingest_latencies.append((time.perf_counter() - t) * 1000.0)
        if args.progress_every and i % args.progress_every == 0:
            elapsed = time.perf_counter() - ingest_start
            print(
                f"  [ingest {i}/{len(docs)}] elapsed {elapsed:.1f}s "
                f"rate {i / elapsed:.1f}/s "
                f"recent-p50 {pct(ingest_latencies[-args.progress_every:], 50):.0f}ms",
                flush=True,
            )
    ingest_total_s = time.perf_counter() - ingest_start

    # ── Recall ────────────────────────────────────────────────────────────
    def norm(s: str) -> str:
        return " ".join(s.lower().split())

    recall_latencies: list[float] = []
    ranks: list[int | None] = []
    recall_start = time.perf_counter()
    for q_idx, (_gold_ctx_id, answers, q) in enumerate(queries, start=1):
        norm_answers = [norm(a) for a in answers if a.strip()]
        t = time.perf_counter()
        hits = await kb.top(args.top_k).search(q)
        recall_latencies.append((time.perf_counter() - t) * 1000.0)
        rank: int | None = None
        for i, h in enumerate(hits, start=1):
            body = norm(h.get("text") or "")
            if any(a in body for a in norm_answers):
                rank = i
                break
        ranks.append(rank)
        if args.progress_every and q_idx % args.progress_every == 0:
            elapsed = time.perf_counter() - recall_start
            print(
                f"  [recall {q_idx}/{len(queries)}] elapsed {elapsed:.1f}s "
                f"rate {q_idx / elapsed:.1f}/s",
                flush=True,
            )
    recall_total_s = time.perf_counter() - recall_start

    sampler.stop()

    # ── Report ────────────────────────────────────────────────────────────
    def recall_at(k: int) -> float:
        hits = sum(1 for r in ranks if r is not None and r <= k)
        return hits / len(ranks) if ranks else 0.0

    mrr = (
        sum(1.0 / r for r in ranks if r is not None) / len(ranks) if ranks else 0.0
    )

    # Moon footprint
    moon_stats: dict[str, object] = {}
    try:
        import redis as _redis  # type: ignore[import-not-found]

        host_port = MOON_URL.split("://", 1)[1]
        host, port = host_port.split(":")
        r = _redis.Redis(host=host, port=int(port))
        moon_stats["dbsize"] = r.dbsize()
        for idx in ("chunks", "entities", "facts", "communities"):
            try:
                info = r.execute_command("FT.INFO", idx)
                flat = {
                    info[i].decode() if isinstance(info[i], bytes) else str(info[i]): info[i + 1]
                    for i in range(0, len(info) - 1, 2)
                }
                moon_stats[f"{idx}_num_docs"] = int(flat.get("num_docs", 0) or 0)
            except Exception as e:
                moon_stats[f"{idx}_num_docs"] = f"error: {e}"
        try:
            info_mem = r.info(section="memory")
            moon_stats["used_memory_human"] = info_mem.get("used_memory_human", "n/a")
            moon_stats["used_memory_bytes"] = info_mem.get("used_memory", 0)
        except Exception:
            pass
    except Exception as e:
        moon_stats["error"] = str(e)

    print("\n════════════ MOON FOOTPRINT ════════════")
    for k, v in moon_stats.items():
        print(f"  {k:<22}: {v}")

    print("\n════════════ INGEST ════════════")
    print(f"  docs              : {len(docs)}")
    print(f"  total wall        : {ingest_total_s:.2f} s")
    print(f"  throughput        : {len(docs) / ingest_total_s:.1f} docs/s")
    print(f"  per-doc p50       : {pct(ingest_latencies, 50):.1f} ms")
    print(f"  per-doc p95       : {pct(ingest_latencies, 95):.1f} ms")
    print(f"  per-doc p99       : {pct(ingest_latencies, 99):.1f} ms")
    print(f"  per-doc max       : {max(ingest_latencies):.1f} ms")

    print("\n════════════ RECALL ════════════")
    print(f"  queries           : {len(queries)}")
    print(f"  total wall        : {recall_total_s:.2f} s")
    print(f"  recall@1          : {recall_at(1):.1%}")
    print(f"  recall@3          : {recall_at(3):.1%}")
    print(f"  recall@5          : {recall_at(5):.1%}")
    print(f"  recall@10         : {recall_at(10):.1%}")
    print(f"  MRR               : {mrr:.3f}")
    print(f"  latency p50       : {pct(recall_latencies, 50):.1f} ms")
    print(f"  latency p95       : {pct(recall_latencies, 95):.1f} ms")
    print(f"  latency p99       : {pct(recall_latencies, 99):.1f} ms")
    print(f"  latency max       : {max(recall_latencies):.1f} ms")
    print(f"  latency mean      : {statistics.mean(recall_latencies):.1f} ms")

    res = sampler.report()
    print("\n════════════ RESOURCE USAGE ════════════")
    print(json.dumps(res, indent=2))

    summary = {
        "backend": MOON_URL,
        "split": args.split,
        "corpus_size": len(docs),
        "queries": len(queries),
        "ingest": {
            "total_s": round(ingest_total_s, 3),
            "docs_per_s": round(len(docs) / ingest_total_s, 2),
            "p50_ms": round(pct(ingest_latencies, 50), 2),
            "p95_ms": round(pct(ingest_latencies, 95), 2),
            "p99_ms": round(pct(ingest_latencies, 99), 2),
            "max_ms": round(max(ingest_latencies), 2),
        },
        "recall": {
            "total_s": round(recall_total_s, 3),
            "recall_at_1": round(recall_at(1), 4),
            "recall_at_3": round(recall_at(3), 4),
            "recall_at_5": round(recall_at(5), 4),
            "recall_at_10": round(recall_at(10), 4),
            "mrr": round(mrr, 4),
            "p50_ms": round(pct(recall_latencies, 50), 2),
            "p95_ms": round(pct(recall_latencies, 95), 2),
            "p99_ms": round(pct(recall_latencies, 99), 2),
            "max_ms": round(max(recall_latencies), 2),
            "mean_ms": round(statistics.mean(recall_latencies), 2),
        },
        "moon": moon_stats,
        "resources": res,
    }
    print("\n════════════ JSON ════════════")
    print(json.dumps(summary, indent=2, default=str))


asyncio.run(main())
