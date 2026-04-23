"""Drop-in Ollama replacement that serves embeddings from a precomputed cache.

Speaks the tiny subset of the Ollama HTTP API that Lunaris consumes:

  POST /api/embed  {"model": "...", "input": [str, ...]}
       -> {"model": "...", "embeddings": [[float, ...], ...]}

  GET  /api/tags
       -> {"models": [{"name": "..."}]}  (for clients that probe)

When Lunaris's `OllamaEmbedder` sends a text that isn't in the cache
we respond with HTTP 404 so the miss is loud, not silently zero'd. Run
real Ollama + `precompute-embeds.py` first; stop Ollama; start this
server on 11434; rerun the benchmark; restart Ollama when done.

Usage:
  uv run --with numpy \
    python scripts/ollama-replay-server.py \
      --cache milestones/v0.1.1-bench/cache/squad-10k.npz \
      --port 11434
"""
from __future__ import annotations

import argparse
import hashlib
import json
import signal
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import numpy as np

MODEL = "embeddinggemma:300m"


def sha(s: str) -> str:
    return hashlib.sha256(s.encode("utf-8")).hexdigest()


class CacheState:
    def __init__(
        self,
        path: Path,
        upstream: str | None = None,
        model: str = MODEL,
    ) -> None:
        self.path = path
        self.upstream = upstream.rstrip("/") if upstream else None
        self.model = model
        self.cache: dict[str, np.ndarray] = {}
        if path.exists():
            with np.load(path, allow_pickle=False) as f:
                hashes = [h.decode() for h in f["hashes"]]
                vectors = f["vectors"]
            self.cache = {h: vectors[i] for i, h in enumerate(hashes)}
        self.hits = 0
        self.misses = 0
        self.recorded = 0
        self.missed_samples: list[str] = []
        self._lock = threading.Lock()

    def lookup(self, text: str) -> np.ndarray | None:
        v = self.cache.get(sha(text))
        if v is None:
            self.misses += 1
            if len(self.missed_samples) < 10:
                self.missed_samples.append(text[:120])
            return None
        self.hits += 1
        return v

    def record_from_upstream(self, text: str) -> np.ndarray:
        """Fetch from the real Ollama, cache, return. Requires `upstream`."""
        import urllib.request

        assert self.upstream is not None, "record mode requires --upstream"
        body = json.dumps({"model": self.model, "input": [text]}).encode("utf-8")
        req = urllib.request.Request(
            f"{self.upstream}/api/embed",
            data=body,
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=30) as resp:
            payload = json.loads(resp.read().decode("utf-8"))
        vec = np.array(payload["embeddings"][0], dtype=np.float32)
        with self._lock:
            self.cache[sha(text)] = vec
            self.recorded += 1
        return vec

    def save(self) -> None:
        if not self.cache:
            return
        hashes = list(self.cache.keys())
        vectors = np.stack([self.cache[h] for h in hashes]).astype(np.float32)
        # `np.savez` auto-appends `.npz` when the destination doesn't end in
        # `.npz`, which broke the atomic-rename pattern (we wrote to
        # `foo.npz.tmp` and numpy saved `foo.npz.tmp.npz`). Hand the final
        # path directly and skip the rename — crashes during save are
        # benign because we're just a cache.
        np.savez(
            str(self.path),
            hashes=np.array([h.encode() for h in hashes]),
            vectors=vectors,
        )


class Handler(BaseHTTPRequestHandler):
    state: CacheState

    def log_message(self, format: str, *args: object) -> None:
        return

    def _json(self, status: int, payload: dict[str, object]) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        if self.path in ("/api/tags", "/api/version"):
            self._json(
                200,
                (
                    {"version": "replay"}
                    if self.path == "/api/version"
                    else {"models": [{"name": MODEL}]}
                ),
            )
            return
        self._json(404, {"error": f"unhandled path {self.path}"})

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/api/embed":
            self._json(404, {"error": f"unhandled path {self.path}"})
            return
        length = int(self.headers.get("Content-Length", "0"))
        try:
            body = json.loads(self.rfile.read(length).decode("utf-8"))
        except Exception as e:
            self._json(400, {"error": f"bad json: {e}"})
            return
        inputs = body.get("input")
        if not isinstance(inputs, list) or not all(isinstance(s, str) for s in inputs):
            self._json(400, {"error": "input must be list[str]"})
            return
        out: list[list[float]] = []
        for text in inputs:
            vec = self.state.lookup(text)
            if vec is None:
                if self.state.upstream:
                    try:
                        vec = self.state.record_from_upstream(text)
                    except Exception as e:
                        self._json(
                            502,
                            {"error": f"upstream fetch failed: {e}"},
                        )
                        return
                else:
                    self._json(
                        404,
                        {
                            "error": "cache miss",
                            "text_preview": text[:160],
                            "hits": self.state.hits,
                            "misses": self.state.misses,
                        },
                    )
                    return
            out.append(vec.astype(float).tolist())
        self._json(200, {"model": MODEL, "embeddings": out})


def port_in_use(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(0.2)
        try:
            s.connect(("127.0.0.1", port))
            return True
        except Exception:
            return False


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--cache", required=True)
    ap.add_argument("--port", type=int, default=11434)
    ap.add_argument("--bind", default="127.0.0.1")
    ap.add_argument(
        "--upstream",
        default=None,
        help="Real Ollama URL — when set, cache misses forward upstream and "
        "are recorded. Must NOT be on the same port as --port (so point it "
        "at e.g. http://localhost:21434 with Ollama moved there).",
    )
    args = ap.parse_args()

    if port_in_use(args.port):
        sys.stderr.write(
            f"[ERROR] port {args.port} is already bound — stop Ollama first "
            f"and retry.\n"
        )
        sys.exit(2)

    cache_path = Path(args.cache)
    if not args.upstream and not cache_path.exists():
        sys.stderr.write(
            f"[ERROR] cache file not found: {cache_path} "
            f"(start with --upstream to record a new cache)\n"
        )
        sys.exit(2)

    state = CacheState(cache_path, upstream=args.upstream)
    Handler.state = state
    mode = "RECORD (proxy to upstream on miss)" if args.upstream else "REPLAY (strict, 404 on miss)"
    print(
        f"[replay] mode={mode} cache={len(state.cache)} entries from {cache_path} "
        f"upstream={args.upstream or '-'} listening on {args.bind}:{args.port}"
    )
    server = ThreadingHTTPServer((args.bind, args.port), Handler)

    def shutdown(signum: int, _frame: object) -> None:
        total = state.hits + state.misses
        print(
            f"\n[replay] shutting down. hits={state.hits} misses={state.misses} "
            f"recorded={state.recorded} "
            f"({(state.hits / total * 100) if total else 0:.1f}% hit rate)"
        )
        if state.recorded > 0:
            print(f"[replay] saving {len(state.cache)} entries to {cache_path}")
            state.save()
        if state.missed_samples:
            print("[replay] first misses:")
            for s in state.missed_samples:
                print(f"  · {s}")
        server.shutdown()

    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)

    def heartbeat() -> None:
        while True:
            time.sleep(30)
            total = state.hits + state.misses
            print(
                f"[replay] hits={state.hits} misses={state.misses} "
                f"({(state.hits / total * 100) if total else 0:.1f}%)",
                flush=True,
            )

    threading.Thread(target=heartbeat, daemon=True).start()
    server.serve_forever()


if __name__ == "__main__":
    main()
