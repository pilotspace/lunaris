#!/usr/bin/env python3
"""Lunaris-via-MCP stdio benchmark.

Spawns the lunaris-mcp binary, completes the JSON-RPC initialize handshake,
then measures cold + warm latencies for tools/list, memory.ingest, and
memory.recall. Reports p50 / p95 / p99 per operation.

Usage:
    python3 scripts/bench-mcp-stdio.py [--n-ingest 100] [--n-recall 100]
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
            text=False,
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
        # Read until we get a response with our id
        while True:
            raw = self.proc.stdout.readline()
            if not raw:
                raise RuntimeError(f"server closed stdout while waiting for {method}")
            msg = json.loads(raw.decode("utf-8"))
            if msg.get("id") == self._req_id:
                if "error" in msg:
                    raise RuntimeError(f"{method} error: {msg['error']}")
                return msg.get("result", {})
            # ignore notifications / out-of-band

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
            self.proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="/Volumes/Games/tindang-repo/lunaris/target/release/lunaris-mcp")
    ap.add_argument("--scope", default="bench-mcp-stdio")
    ap.add_argument("--storage", default="sqlite:///tmp/lunaris-bench-mcp.db")
    ap.add_argument("--gguf", default="/Users/tindang/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf")
    ap.add_argument("--n-ingest", type=int, default=100)
    ap.add_argument("--n-recall", type=int, default=100)
    args = ap.parse_args()

    # Reset bench DB so we measure consistent state
    for ext in ("", "-shm", "-wal"):
        p = Path("/tmp") / f"lunaris-bench-mcp.db{ext}"
        if p.exists():
            p.unlink()

    env = {
        "LUNARIS_MCP_SCOPE": args.scope,
        "LUNARIS_MCP_STORAGE": args.storage,
        "LUNARIS_EMBEDDER_GGUF": args.gguf,
        "LUNARIS_MCP_LOG": "warn",
    }

    print(f"=== Lunaris-via-MCP stdio benchmark ===")
    print(f"binary:  {args.binary}")
    print(f"scope:   {args.scope}")
    print(f"storage: {args.storage}")
    print(f"ingest:  {args.n_ingest} ops")
    print(f"recall:  {args.n_recall} ops")
    print()

    # === Cold start (boot + initialize + tools/list) ===
    t_cold_start = time.perf_counter()
    client = McpClient(Path(args.binary), env)
    t_spawn = time.perf_counter()
    client.call(
        "initialize",
        {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "bench-mcp-stdio", "version": "0.1.0"},
        },
    )
    t_init = time.perf_counter()
    client.notify("notifications/initialized")
    tools = client.call("tools/list", {})
    t_tools = time.perf_counter()

    cold_total_ms = (t_tools - t_cold_start) * 1000
    cold_spawn_ms = (t_spawn - t_cold_start) * 1000
    cold_init_ms = (t_init - t_spawn) * 1000
    cold_tools_ms = (t_tools - t_init) * 1000
    tool_count = len(tools.get("tools", []))
    print(f"COLD START")
    print(f"  spawn        {cold_spawn_ms:7.1f} ms  (process + bootstrap + embedder load)")
    print(f"  initialize   {cold_init_ms:7.1f} ms")
    print(f"  tools/list   {cold_tools_ms:7.1f} ms  ({tool_count} tools)")
    print(f"  TOTAL        {cold_total_ms:7.1f} ms")
    print()

    # === memory.ingest ===
    ingest_lat: list[float] = []
    for i in range(args.n_ingest):
        t0 = time.perf_counter()
        client.call(
            "tools/call",
            {
                "name": "memory.ingest",
                "arguments": {
                    "source": f"bench/ingest-{i}",
                    "content": f"Lunaris benchmark sample {i}: measuring memory.ingest p50/p95/p99 over stdio JSON-RPC.",
                },
            },
        )
        ingest_lat.append((time.perf_counter() - t0) * 1000)

    print(f"memory.ingest  (n={args.n_ingest})")
    print(f"  p50  {percentile(ingest_lat, 50):7.1f} ms")
    print(f"  p95  {percentile(ingest_lat, 95):7.1f} ms")
    print(f"  p99  {percentile(ingest_lat, 99):7.1f} ms")
    print(f"  max  {max(ingest_lat):7.1f} ms")
    print(f"  mean {statistics.mean(ingest_lat):7.1f} ms")
    print()

    # === memory.recall ===
    recall_lat: list[float] = []
    queries = [
        "Lunaris benchmark sample",
        "memory.ingest p50",
        "stdio JSON-RPC",
        "measuring",
        "sample 42",
    ]
    hit_counts: list[int] = []
    for i in range(args.n_recall):
        q = queries[i % len(queries)]
        t0 = time.perf_counter()
        result = client.call(
            "tools/call",
            {"name": "memory.recall", "arguments": {"query": q, "k": 5}},
        )
        recall_lat.append((time.perf_counter() - t0) * 1000)
        # extract hits count from MCP envelope
        try:
            content = json.loads(result["content"][0]["text"])
            hit_counts.append(len(content.get("hits", [])))
        except Exception:
            hit_counts.append(-1)

    print(f"memory.recall  (n={args.n_recall}, k=5)")
    print(f"  p50  {percentile(recall_lat, 50):7.1f} ms")
    print(f"  p95  {percentile(recall_lat, 95):7.1f} ms")
    print(f"  p99  {percentile(recall_lat, 99):7.1f} ms")
    print(f"  max  {max(recall_lat):7.1f} ms")
    print(f"  mean {statistics.mean(recall_lat):7.1f} ms")
    avg_hits = sum(h for h in hit_counts if h >= 0) / max(1, sum(1 for h in hit_counts if h >= 0))
    print(f"  avg hits/query {avg_hits:.1f}")
    print()

    client.close()
    print(f"=== benchmark complete ===")
    return 0


if __name__ == "__main__":
    sys.exit(main())
