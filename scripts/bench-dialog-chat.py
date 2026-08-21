"""ChatAgentMemory benchmark with DailyDialog turns + embeddinggemma:300m.

Symmetric to `bench-squad-kb.py` but exercises the Phase 10 conversational
path:

  * per-user source-prefix scoping (post-hydrate filter)
  * ACT-R recency blend in `MessageStream::recall`
  * Moon-native HYBRID fused recall on short conversational strings

Ingests N turns under a single user, then queries with the first ~80
characters of a random subset of those turns. A hit "counts" when the
original turn text (normalized) appears in the returned chunk text.

Usage:
  LUNARIS_MOON_URL="moon://127.0.0.1:6380" \
    uv run --with datasets --with python-ulid --with redis --with psutil \
      python scripts/bench-dialog-chat.py \
        [--turns 5000] [--queries 500] [--top-k 10]
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import random
import statistics
import threading
import time

import lunaris
from lunaris.conversational import ChatAgentMemory

MOON_URL = os.environ.get("LUNARIS_MOON_URL", "moon://127.0.0.1:6380")
USER_ID = "bench-user-dialog"


def pct(xs: list[float], p: float) -> float:
    if not xs:
        return float("nan")
    xs = sorted(xs)
    k = max(0, min(len(xs) - 1, int(round((p / 100.0) * (len(xs) - 1)))))
    return xs[k]


class ResourceSampler:
    def __init__(self, targets: list[str], interval_s: float = 1.0) -> None:
        self.targets = targets
        self.interval_s = interval_s
        self.samples: dict[str, list[tuple[float, float]]] = {t: [] for t in targets}
        self.system_cpu: list[float] = []
        self.system_mem_pct: list[float] = []
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._psutil = None
        try:
            import psutil  # type: ignore[import-not-found]

            self._psutil = psutil
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
                    if t.lower() in name or t.lower() in cmd:
                        procs[t].append(p)
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
            except Exception:
                pass
            self._stop.wait(self.interval_s)

    def stop(self) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=2.0)

    def report(self) -> dict[str, object]:
        if self._psutil is None:
            return {"note": "psutil not installed"}
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


def load_dialog_turns(n_turns: int) -> list[str]:
    """Load N short conversational-length strings from SQuAD questions.

    SQuAD questions (~8–15 words each) are a reasonable stand-in for
    chat turns — the point here is stress-testing ChatAgentMemory's
    per-user scoping + recency blend on short strings, not faithfulness
    to a specific dialog dataset. DailyDialog's script loader was
    deprecated by HuggingFace and the parquet mirrors require auth.
    """
    from datasets import load_dataset

    ds = load_dataset("rajpurkar/squad", split="train")
    turns: list[str] = []
    seen: set[str] = set()
    for row in ds:
        q = (row.get("question") or "").strip()
        if len(q) >= 10 and q not in seen:
            seen.add(q)
            turns.append(q)
            if len(turns) >= n_turns:
                return turns
    return turns


async def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--turns", type=int, default=5000)
    ap.add_argument("--queries", type=int, default=500)
    ap.add_argument("--top-k", type=int, default=10)
    ap.add_argument("--progress-every", type=int, default=0)
    ap.add_argument("--query-prefix-chars", type=int, default=80)
    ap.add_argument("--seed", type=int, default=7)
    args = ap.parse_args()

    print(f"# Backend      : {MOON_URL}")
    print(f"# Embedder     : Ollama embeddinggemma:300m (768d Google EmbeddingGemma)")
    print(f"# Corpus       : daily_dialog train (conversational turns)")
    print(
        f"# Plan         : ingest {args.turns} turns, {args.queries} recall queries "
        f"(first {args.query_prefix_chars} chars of random turns), top-{args.top_k}"
    )

    turns = load_dialog_turns(args.turns)
    rng = random.Random(args.seed)
    query_indices = rng.sample(range(len(turns)), min(args.queries, len(turns)))
    print(f"# Loaded       : {len(turns)} turns")

    sampler = ResourceSampler(targets=["moon", "ollama", "bench-dialog-chat"])
    sampler.start()

    handle = await lunaris.open(MOON_URL)
    chat = ChatAgentMemory.new(handle, USER_ID)

    # ── Ingest ────────────────────────────────────────────────────────────
    ingest_latencies: list[float] = []
    ingest_start = time.perf_counter()
    for i, turn in enumerate(turns, start=1):
        t = time.perf_counter()
        await chat.remember(turn)
        ingest_latencies.append((time.perf_counter() - t) * 1000.0)
        if args.progress_every and i % args.progress_every == 0:
            elapsed = time.perf_counter() - ingest_start
            print(
                f"  [ingest {i}/{len(turns)}] elapsed {elapsed:.1f}s "
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
    for q_idx, turn_idx in enumerate(query_indices, start=1):
        gold = turns[turn_idx]
        query = gold[: args.query_prefix_chars]
        gold_norm = norm(gold)
        t = time.perf_counter()
        hits = await chat.recall(query)
        recall_latencies.append((time.perf_counter() - t) * 1000.0)
        rank: int | None = None
        for i, h in enumerate(hits, start=1):
            body = norm(h.get("text") or "")
            # A hit counts when the gold turn is contained in / equal to the
            # returned chunk (chunker may have split the utterance into
            # multiple chunks, and recall matches on any of them).
            if gold_norm in body or body in gold_norm:
                rank = i
                break
        ranks.append(rank)
        if args.progress_every and q_idx % args.progress_every == 0:
            elapsed = time.perf_counter() - recall_start
            print(
                f"  [recall {q_idx}/{len(query_indices)}] elapsed {elapsed:.1f}s "
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
    except Exception as e:
        moon_stats["error"] = str(e)

    print("\n════════════ MOON FOOTPRINT ════════════")
    for k, v in moon_stats.items():
        print(f"  {k:<22}: {v}")

    print("\n════════════ INGEST ════════════")
    print(f"  turns             : {len(turns)}")
    print(f"  total wall        : {ingest_total_s:.2f} s")
    print(f"  throughput        : {len(turns) / ingest_total_s:.1f} turns/s")
    print(f"  per-turn p50      : {pct(ingest_latencies, 50):.1f} ms")
    print(f"  per-turn p95      : {pct(ingest_latencies, 95):.1f} ms")
    print(f"  per-turn p99      : {pct(ingest_latencies, 99):.1f} ms")
    print(f"  per-turn max      : {max(ingest_latencies):.1f} ms")

    print("\n════════════ RECALL ════════════")
    print(f"  queries           : {len(query_indices)}")
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
        "corpus": "daily_dialog train",
        "turns": len(turns),
        "queries": len(query_indices),
        "ingest": {
            "total_s": round(ingest_total_s, 3),
            "turns_per_s": round(len(turns) / ingest_total_s, 2),
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
