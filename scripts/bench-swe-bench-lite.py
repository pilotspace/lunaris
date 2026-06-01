#!/usr/bin/env python3
"""SWE-bench Lite retrieval-quality benchmark for Lunaris via MCP stdio.

For each of N instances pulled from princeton-nlp/SWE-bench_Lite (test split):

  Ingest:  source = swe-bench/<instance_id>
           content = problem_statement + "\n\n---\n\n" + hints_text

  Query:   first line of problem_statement (mimics a developer's search-box query)
           k = 10

  Score:   recall@1 / @3 / @5 / @10 + MRR over whether the gold instance_id
           appears in the recalled source strings

Runs against whatever storage backend lunaris-mcp is configured for
(LUNARIS_MCP_STORAGE) — SQLite, Moon, or Postgres.

Usage:
    .../python scripts/bench-swe-bench-lite.py --n 100 [--binary ...] [--storage ...]
"""
from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path


def percentile(values: list[float], p: float) -> float:
    if not values:
        return 0.0
    s = sorted(values)
    k = (len(s) - 1) * (p / 100.0)
    f = int(k)
    c = min(f + 1, len(s) - 1)
    if f == c:
        return s[f]
    return s[f] + (s[c] - s[f]) * (k - f)


class McpClient:
    def __init__(self, binary: Path, env: dict[str, str]):
        merged_env = {**os.environ, **env}
        self.proc = subprocess.Popen(
            [str(binary)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=merged_env,
            bufsize=0,
        )
        self._req_id = 0

    def call(self, method: str, params: dict | None = None) -> dict:
        self._req_id += 1
        req = {"jsonrpc": "2.0", "id": self._req_id, "method": method}
        if params is not None:
            req["params"] = params
        line = (json.dumps(req) + "\n").encode("utf-8")
        self.proc.stdin.write(line)
        self.proc.stdin.flush()
        while True:
            raw = self.proc.stdout.readline()
            if not raw:
                raise RuntimeError(f"server closed stdout while waiting for {method}")
            msg = json.loads(raw.decode("utf-8"))
            if msg.get("id") == self._req_id:
                if "error" in msg:
                    raise RuntimeError(f"{method} error: {msg['error']}")
                return msg.get("result", {})

    def notify(self, method: str, params: dict | None = None) -> None:
        req = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            req["params"] = params
        line = (json.dumps(req) + "\n").encode("utf-8")
        self.proc.stdin.write(line)
        self.proc.stdin.flush()

    def close(self) -> None:
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def parse_tool_result(result: dict) -> dict:
    """Extract the JSON payload from an MCP tools/call envelope."""
    content = result.get("content")
    if isinstance(content, list) and content:
        text = content[0].get("text", "")
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            return {"_raw": text}
    return result


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--binary",
        default="/Volumes/Games/tindang-repo/lunaris/target/release/lunaris-mcp",
    )
    ap.add_argument("--scope", default="bench-swe-bench-lite")
    ap.add_argument("--storage", default=None, help="LUNARIS_MCP_STORAGE override")
    ap.add_argument(
        "--gguf",
        default="/Users/tindang/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf",
    )
    ap.add_argument("--n", type=int, default=100, help="number of SWE-bench instances to ingest+query")
    ap.add_argument(
        "--max-content-chars",
        type=int,
        default=4000,
        help="truncate ingested content to N chars to keep chunk count bounded (0 = no truncation)",
    )
    ap.add_argument(
        "--dataset",
        default="princeton-nlp/SWE-bench_Lite",
        help="HF dataset name",
    )
    ap.add_argument("--split", default="test", help="HF dataset split")
    ap.add_argument(
        "--out",
        default="/tmp/lunaris-swe-bench-lite-results.json",
        help="JSON results file path",
    )
    ap.add_argument(
        "--source-prefix",
        default="swe-bench",
        help="source prefix to use for ingest+gold lookup (change to bust dedupe within a scope)",
    )
    args = ap.parse_args()

    # === Load SWE-bench Lite ===
    print(f"=== loading {args.dataset} (split={args.split}) ===")
    t0 = time.perf_counter()
    from datasets import load_dataset  # noqa: WPS433

    ds = load_dataset(args.dataset, split=args.split)
    print(f"loaded {len(ds)} instances in {time.perf_counter() - t0:.1f}s — using first {args.n}")
    ds = ds.select(range(min(args.n, len(ds))))

    # === Boot lunaris-mcp ===
    env = {
        "LUNARIS_MCP_SCOPE": args.scope,
        "LUNARIS_EMBEDDER_GGUF": args.gguf,
        "LUNARIS_MCP_LOG": "warn",
    }
    if args.storage:
        env["LUNARIS_MCP_STORAGE"] = args.storage

    print(f"=== Lunaris-via-MCP SWE-bench Lite benchmark ===")
    print(f"binary:  {args.binary}")
    print(f"scope:   {args.scope}")
    print(f"storage: {env.get('LUNARIS_MCP_STORAGE', '(default from env / config)')}")
    print(f"N:       {len(ds)}")
    print()

    t_cold_start = time.perf_counter()
    client = McpClient(Path(args.binary), env)
    client.call(
        "initialize",
        {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "bench-swe-bench-lite", "version": "0.1.0"},
        },
    )
    client.notify("notifications/initialized")
    client.call("tools/list", {})
    cold_ms = (time.perf_counter() - t_cold_start) * 1000
    print(f"cold start: {cold_ms:.1f} ms")
    print()

    # === Ingest ===
    print("ingesting...")
    ingest_lat: list[float] = []
    ingested_sources: list[str] = []
    for i, row in enumerate(ds):
        instance_id = row["instance_id"]
        problem = row.get("problem_statement", "") or ""
        hints = row.get("hints_text", "") or ""
        content = problem
        if hints.strip():
            content += "\n\n---\n\n" + hints
        if args.max_content_chars > 0 and len(content) > args.max_content_chars:
            content = content[: args.max_content_chars]
        source = f"{args.source_prefix}/{instance_id}"
        t0 = time.perf_counter()
        client.call(
            "tools/call",
            {
                "name": "memory.ingest",
                "arguments": {"source": source, "content": content},
            },
        )
        ingest_lat.append((time.perf_counter() - t0) * 1000)
        ingested_sources.append(source)
        if (i + 1) % 20 == 0:
            print(f"  ingested {i+1}/{len(ds)}  (last p50={percentile(ingest_lat, 50):.1f}ms)", flush=True)

    print()
    print(f"ingest (n={len(ingest_lat)})")
    print(f"  p50  {percentile(ingest_lat, 50):7.1f} ms")
    print(f"  p95  {percentile(ingest_lat, 95):7.1f} ms")
    print(f"  p99  {percentile(ingest_lat, 99):7.1f} ms")
    print(f"  mean {statistics.mean(ingest_lat):7.1f} ms")
    print()

    # === Query: first line of problem_statement, expect gold source in top-k ===
    print("querying...")
    recall_lat: list[float] = []
    ranks: list[int] = []  # 1-indexed rank of gold; 0 if missed
    per_instance = []
    for i, row in enumerate(ds):
        instance_id = row["instance_id"]
        gold_source = f"{args.source_prefix}/{instance_id}"
        problem = (row.get("problem_statement") or "").strip()
        # use first non-empty line as the query (mimics a search-box)
        first_line = ""
        for line in problem.splitlines():
            line = line.strip()
            if line and not line.startswith("#"):
                first_line = line
                break
        if not first_line:
            first_line = problem[:200]
        # Cap query length to ~512 chars (1 model context window safety)
        query = first_line[:512]

        t0 = time.perf_counter()
        result = client.call(
            "tools/call",
            {"name": "memory.recall", "arguments": {"query": query, "k": 10}},
        )
        recall_lat.append((time.perf_counter() - t0) * 1000)

        payload = parse_tool_result(result)
        hits = payload.get("hits", []) if isinstance(payload, dict) else []
        rank = 0
        for r, hit in enumerate(hits, start=1):
            if hit.get("source") == gold_source:
                rank = r
                break
        ranks.append(rank)
        per_instance.append({
            "instance_id": instance_id,
            "query": query[:120],
            "gold_source": gold_source,
            "rank": rank,
            "hit_count": len(hits),
            "latency_ms": recall_lat[-1],
        })
        if (i + 1) % 20 == 0:
            r1 = sum(1 for r in ranks if r == 1) / len(ranks)
            print(f"  queried {i+1}/{len(ds)}  recall@1={r1:.2%}  last_p50={percentile(recall_lat, 50):.1f}ms", flush=True)

    print()

    # === Aggregate ===
    n = len(ranks)
    def at_k(k):
        return sum(1 for r in ranks if 1 <= r <= k) / n
    mrr = sum((1.0 / r if r > 0 else 0.0) for r in ranks) / n

    print(f"recall (n={n}, k=10)")
    print(f"  p50  {percentile(recall_lat, 50):7.1f} ms")
    print(f"  p95  {percentile(recall_lat, 95):7.1f} ms")
    print(f"  p99  {percentile(recall_lat, 99):7.1f} ms")
    print(f"  mean {statistics.mean(recall_lat):7.1f} ms")
    print()
    print(f"retrieval quality")
    print(f"  recall@1   {at_k(1):.2%}")
    print(f"  recall@3   {at_k(3):.2%}")
    print(f"  recall@5   {at_k(5):.2%}")
    print(f"  recall@10  {at_k(10):.2%}")
    print(f"  MRR        {mrr:.4f}")
    print(f"  misses     {sum(1 for r in ranks if r == 0)}/{n}")
    print()

    # === Save report ===
    report = {
        "dataset": args.dataset,
        "split": args.split,
        "n": n,
        "storage": env.get("LUNARIS_MCP_STORAGE", "(default)"),
        "scope": args.scope,
        "cold_start_ms": cold_ms,
        "ingest": {
            "p50_ms": percentile(ingest_lat, 50),
            "p95_ms": percentile(ingest_lat, 95),
            "p99_ms": percentile(ingest_lat, 99),
            "mean_ms": statistics.mean(ingest_lat),
        },
        "recall": {
            "p50_ms": percentile(recall_lat, 50),
            "p95_ms": percentile(recall_lat, 95),
            "p99_ms": percentile(recall_lat, 99),
            "mean_ms": statistics.mean(recall_lat),
        },
        "quality": {
            "recall_at_1": at_k(1),
            "recall_at_3": at_k(3),
            "recall_at_5": at_k(5),
            "recall_at_10": at_k(10),
            "mrr": mrr,
            "misses": sum(1 for r in ranks if r == 0),
        },
        "per_instance": per_instance,
    }
    Path(args.out).write_text(json.dumps(report, indent=2))
    print(f"report saved: {args.out}")

    client.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
