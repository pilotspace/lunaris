#!/usr/bin/env python3
"""Lunaris-via-MCP stdio benchmark.

Spawns the lunaris-mcp binary, completes the JSON-RPC initialize handshake,
then measures cold + warm latencies for tools/list, memory.status,
memory.ingest, and memory.recall. Reports p50 / p95 / p99 per operation.

Usage:
    python3 scripts/bench-mcp-stdio.py [--n-ingest 100] [--n-recall 100]
"""
from __future__ import annotations

import argparse
import json
import os
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MCP_BIN = ROOT / "target" / "release" / "lunaris-mcp"
DEFAULT_MOON_BIN = ROOT / "vendor" / "moon" / "target" / "release" / "moon"
DEFAULT_GGUF = Path.home() / ".lunaris" / "models" / "granite-embedding-311m-multilingual-r2.Q4_K_M.gguf"


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


def tool_json(result: dict) -> dict:
    return json.loads(result["content"][0]["text"])


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


def tcp_ready(host: str, port: int, timeout_s: float) -> bool:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=0.2):
                return True
        except OSError:
            time.sleep(0.05)
    return False


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def reset_sqlite_storage(storage: str) -> None:
    if not storage.startswith("sqlite://"):
        return
    path = storage[len("sqlite://") :]
    if path.startswith("/"):
        db = Path(path)
    else:
        db = Path(path).resolve()
    for ext in ("", "-shm", "-wal"):
        p = Path(f"{db}{ext}")
        if p.exists():
            p.unlink()


def start_moon(binary: Path, port: int, data_dir: Path) -> subprocess.Popen[bytes]:
    proc = subprocess.Popen(
        [
            str(binary),
            "--bind",
            "127.0.0.1",
            "--port",
            str(port),
            "--admin-port",
            "0",
            "--dir",
            str(data_dir),
            "--appendonly",
            "no",
            "--shards",
            "1",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if not tcp_ready("127.0.0.1", port, timeout_s=5.0):
        proc.terminate()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()
        raise RuntimeError(f"Moon did not become ready on 127.0.0.1:{port}")
    return proc


def stop_proc(proc: subprocess.Popen[bytes] | None) -> None:
    if proc is None:
        return
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=str(DEFAULT_MCP_BIN))
    ap.add_argument("--scope", default="bench-mcp-stdio")
    ap.add_argument("--storage", default="sqlite:///tmp/lunaris-bench-mcp.db")
    ap.add_argument("--gguf", default=str(DEFAULT_GGUF))
    ap.add_argument("--n-ingest", type=int, default=100)
    ap.add_argument("--n-recall", type=int, default=100)
    ap.add_argument("--warmup-ingest", type=int, default=3, help="memory.ingest calls excluded from reported stats.")
    ap.add_argument("--warmup-recall", type=int, default=5, help="memory.recall calls excluded from reported stats.")
    ap.add_argument(
        "--ingest-content-mode",
        choices=("unique", "repeat"),
        default="unique",
        help="Use unique content per ingest, or repeat one content string to expose embed-cache/storage cost.",
    )
    ap.add_argument(
        "--start-moon",
        action="store_true",
        help="Start the vendored Moon server on a temporary port and benchmark against it.",
    )
    ap.add_argument("--moon-binary", default=str(DEFAULT_MOON_BIN))
    ap.add_argument("--moon-port", type=int, default=0, help="Moon port for --start-moon; 0 picks a free port.")
    ap.add_argument("--moon-dir", default=None, help="Moon data dir for --start-moon; default is a temp dir.")
    ap.add_argument(
        "--skip-stage",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Set LUNARIS_MCP_SKIP_STAGE=1 so recall benchmarking excludes GGUF download/hash staging.",
    )
    args = ap.parse_args()

    moon_proc: subprocess.Popen[bytes] | None = None
    moon_tmp: tempfile.TemporaryDirectory[str] | None = None

    if args.start_moon:
        moon_binary = Path(args.moon_binary)
        if not moon_binary.exists():
            raise SystemExit(f"Moon binary not found: {moon_binary}")
        port = args.moon_port or free_port()
        if args.moon_dir:
            moon_dir = Path(args.moon_dir)
            moon_dir.mkdir(parents=True, exist_ok=True)
        else:
            moon_tmp = tempfile.TemporaryDirectory(prefix="lunaris-moon-bench.")
            moon_dir = Path(moon_tmp.name)
        moon_proc = start_moon(moon_binary, port, moon_dir)
        args.storage = f"moon://127.0.0.1:{port}"

    reset_sqlite_storage(args.storage)

    env = {
        "LUNARIS_MCP_SCOPE": args.scope,
        "LUNARIS_MCP_STORAGE": args.storage,
        "LUNARIS_EMBEDDER_GGUF": args.gguf,
        "LUNARIS_MCP_LOG": "warn",
    }
    if args.skip_stage:
        env["LUNARIS_MCP_SKIP_STAGE"] = "1"
    if args.storage.startswith("moon://"):
        env["LUNARIS_GRAPH_ENABLED"] = "1"

    print(f"=== Lunaris-via-MCP stdio benchmark ===")
    print(f"binary:  {args.binary}")
    print(f"scope:   {args.scope}")
    print(f"storage: {args.storage}")
    if args.start_moon:
        print(f"moon:    {args.moon_binary}")
    print(f"ingest:  {args.n_ingest} ops")
    print(f"content: {args.ingest_content_mode}")
    print(f"recall:  {args.n_recall} ops")
    print(f"warmup:  ingest={args.warmup_ingest}, recall={args.warmup_recall}")
    print(f"stage:   {'skipped' if args.skip_stage else 'enabled'}")
    print()

    client: McpClient | None = None
    try:
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
        ingest_total = args.warmup_ingest + args.n_ingest
        for i in range(ingest_total):
            if args.ingest_content_mode == "repeat":
                content = (
                    "Lunaris benchmark repeated sample: measuring memory.ingest storage, "
                    "MQ, and assembly cost when embedding is served from cache."
                )
            else:
                content = (
                    f"Lunaris benchmark sample {i}: measuring memory.ingest p50/p95/p99 "
                    "over stdio JSON-RPC."
                )
            t0 = time.perf_counter()
            client.call(
                "tools/call",
                {
                    "name": "memory.ingest",
                    "arguments": {
                        "source": f"bench/ingest-{i}",
                        "content": content,
                    },
                },
            )
            elapsed_ms = (time.perf_counter() - t0) * 1000
            if i >= args.warmup_ingest:
                ingest_lat.append(elapsed_ms)

        print(f"memory.ingest  (n={args.n_ingest}, warmup={args.warmup_ingest})")
        print(f"  p50  {percentile(ingest_lat, 50):7.1f} ms")
        print(f"  p95  {percentile(ingest_lat, 95):7.1f} ms")
        print(f"  p99  {percentile(ingest_lat, 99):7.1f} ms")
        print(f"  max  {max(ingest_lat):7.1f} ms")
        print(f"  mean {statistics.mean(ingest_lat):7.1f} ms")
        print()

        # === memory.status ===
        t0 = time.perf_counter()
        status_result = client.call("tools/call", {"name": "memory.status", "arguments": {}})
        status_ms = (time.perf_counter() - t0) * 1000
        status = tool_json(status_result)
        queue_summary = ", ".join(
            f"{q.get('topic')}={'ok' if q.get('available') else 'unavailable'}"
            f":{q.get('depth') if q.get('depth') is not None else q.get('error')}"
            for q in status.get("queues", [])
        )
        print("memory.status")
        print(f"  latency       {status_ms:7.1f} ms")
        print(f"  queue_native  {status.get('queue_native')}")
        print(f"  graph_native  {status.get('graph_native')}")
        print(f"  native_rrf    {status.get('native_rrf')}")
        print(f"  queues        {queue_summary}")
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
        recall_total = args.warmup_recall + args.n_recall
        for i in range(recall_total):
            q = queries[i % len(queries)]
            t0 = time.perf_counter()
            result = client.call(
                "tools/call",
                {"name": "memory.recall", "arguments": {"query": q, "k": 5}},
            )
            elapsed_ms = (time.perf_counter() - t0) * 1000
            # extract hits count from MCP envelope
            if i >= args.warmup_recall:
                recall_lat.append(elapsed_ms)
                try:
                    content = tool_json(result)
                    hit_counts.append(len(content.get("hits", [])))
                except Exception:
                    hit_counts.append(-1)

        print(f"memory.recall  (n={args.n_recall}, warmup={args.warmup_recall}, k=5)")
        print(f"  p50  {percentile(recall_lat, 50):7.1f} ms")
        print(f"  p95  {percentile(recall_lat, 95):7.1f} ms")
        print(f"  p99  {percentile(recall_lat, 99):7.1f} ms")
        print(f"  max  {max(recall_lat):7.1f} ms")
        print(f"  mean {statistics.mean(recall_lat):7.1f} ms")
        avg_hits = sum(h for h in hit_counts if h >= 0) / max(1, sum(1 for h in hit_counts if h >= 0))
        print(f"  avg hits/query {avg_hits:.1f}")
        print()
    finally:
        if client is not None:
            client.close()
        stop_proc(moon_proc)
        if moon_tmp is not None:
            moon_tmp.cleanup()

    print(f"=== benchmark complete ===")
    return 0


if __name__ == "__main__":
    sys.exit(main())
