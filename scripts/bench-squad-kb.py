"""Larger live-Moon benchmark over SQuAD paragraphs with embeddinggemma:300m.

Loads N unique contexts from rajpurkar/squad validation, ingests them through
DocumentKnowledgeBase, then issues M queries whose gold paragraph is known,
measuring:

  Ingest
    total wall  |  docs/sec  |  per-doc p50/p95/p99 latency

  Recall
    per-query p50/p95/p99 latency
    recall@1 / @3 / @5 / @10
    MRR (mean reciprocal rank of the gold doc)

Usage:
  LUNARIS_TEST_MOON_URL="moon://127.0.0.1:6380" \
    uv run --with datasets --with python-ulid \
      python scripts/bench-squad-kb.py [--docs 300] [--queries 100]
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import statistics
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


def load_squad(
    n_docs: int, n_queries: int
) -> tuple[list[tuple[str, str]], list[tuple[str, list[str], str]]]:
    """Return (docs, queries).

    docs    = [(ctx_id, context_text)]
    queries = [(ctx_id, answer_spans, question_text)] — answer_spans are the
              SQuAD gold answer strings; a top-k hit "counts" when any span
              is a substring of the hit's chunk text.
    """
    from datasets import load_dataset

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
    docs = [(ctx_ids[c], c) for c in by_ctx]
    queries: list[tuple[str, list[str], str]] = []
    cursor = [list(qs) for qs in by_ctx.values()]
    ctx_list = list(by_ctx.keys())
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
    args = ap.parse_args()

    print(f"# Backend      : {MOON_URL}")
    print(f"# Embedder     : Ollama embeddinggemma:300m (768d)")
    print(f"# Corpus       : rajpurkar/squad validation")
    print(f"# Plan         : ingest {args.docs} paragraphs, query {args.queries} times, top-{args.top_k}")

    docs, queries = load_squad(args.docs, args.queries)
    print(f"# Loaded       : {len(docs)} unique contexts, {len(queries)} queries")

    handle = await lunaris.open(MOON_URL)
    kb = DocumentKnowledgeBase.new(handle, SOURCE_PREFIX)

    # ── Ingest ────────────────────────────────────────────────────────────
    ingest_latencies: list[float] = []
    t0 = time.perf_counter()
    # DocumentKnowledgeBase.ingest takes a batch; time per call then divide
    # by batch-size so per-doc latency is honest. We use batches of 1 so
    # the latency distribution isn't hidden by batching.
    for ctx_id, context in docs:
        meta = {"doc_id": ctx_id, "title": ctx_id}
        t = time.perf_counter()
        await kb.ingest([(context, meta)])
        ingest_latencies.append((time.perf_counter() - t) * 1000.0)
    ingest_total_s = time.perf_counter() - t0

    # ── Recall ────────────────────────────────────────────────────────────
    # Ground truth: a hit "counts" when any of the SQuAD answer spans for the
    # question appears as a substring of the hit chunk text (case-insensitive,
    # whitespace-normalized so chunker tokenization differences don't matter).
    def norm(s: str) -> str:
        return " ".join(s.lower().split())

    recall_latencies: list[float] = []
    ranks: list[int | None] = []
    for _gold_ctx_id, answers, q in queries:
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

    # ── Report ────────────────────────────────────────────────────────────
    def recall_at(k: int) -> float:
        hits = sum(1 for r in ranks if r is not None and r <= k)
        return hits / len(ranks) if ranks else 0.0

    mrr = (
        sum(1.0 / r for r in ranks if r is not None) / len(ranks) if ranks else 0.0
    )

    # ── Moon footprint ─────────────────────────────────────────────────────
    # Query Moon directly for index + memory stats (redis protocol, read-only).
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

    # Structured summary for automation.
    summary = {
        "backend": MOON_URL,
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
            "recall_at_1": round(recall_at(1), 4),
            "recall_at_3": round(recall_at(3), 4),
            "recall_at_5": round(recall_at(5), 4),
            "recall_at_10": round(recall_at(10), 4),
            "mrr": round(mrr, 4),
            "p50_ms": round(pct(recall_latencies, 50), 2),
            "p95_ms": round(pct(recall_latencies, 95), 2),
            "p99_ms": round(pct(recall_latencies, 99), 2),
            "max_ms": round(max(recall_latencies), 2),
        },
    }
    print("\n════════════ JSON ════════════")
    print(json.dumps(summary, indent=2))


asyncio.run(main())
