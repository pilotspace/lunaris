#!/usr/bin/env python3
"""A deterministic OpenAI-compatible `/v1/embeddings` server for CI.

## Why the live-Moon job needs one

`resolve_embedder` falls through to `NoopEmbedder` — zero vectors — when no
GGUF is staged and no remote embedder is configured. No CI runner stages a
GGUF, so every binding suite in `conformance-bindings.yml` was recalling over
an index where **every document sits at the same distance from every query**.
Top-k is then arbitrary, and `source_prefix` is enforced as a post-filter on
the hydrated rows (PR #85), so a recall that legitimately has ten matching
turns returns whichever unrelated rows the index happened to hand back.

That made three of the conversational suites pass and two fail on the same
run, for the same reason: a coin flip. `chat_agent_memory` and
`email_threading` came up tails.

## What this gives instead

A bag-of-tokens embedding: each alphanumeric token hashes (blake2b, 4 bytes)
to one of 768 dimensions and adds 1.0 there; the vector is L2-normalised.
Overlapping vocabulary produces a positive cosine, disjoint vocabulary
produces exactly 0. Ranking assertions become exact rather than probabilistic,
and they stay exact as the shared `_dev_` scope fills up with other suites'
episodes — an unrelated document scores 0 against the query no matter how many
of them there are.

768 is not arbitrary: it matches BOTH `lunaris_embed_remote::openai::
DEFAULT_DIM` and `lunaris_core::NOOP_DEFAULT_DIM`, so a Moon index created by
an earlier no-embedder run has the same dimension and does not need dropping.

This deliberately mirrors `_embed()` in `crates/lunaris-py/tests/
test_query_dsl.py` and `embed()` in `crates/lunaris-ts/__test__/
readme_quickstart.spec.mts`. Those two stand up their own per-test servers and
assert the engine CALLED them (the spy is the point there — it proves the
query text crossed the FFI), so they set `LUNARIS_EMBEDDER_OPENAI_URL` in the
child environment and override this one. Keep the three implementations in
step: a suite that ingests under one and recalls under another gets garbage.

Usage:
    python3 scripts/ci/stub_embedder.py --port 8399 &
    export LUNARIS_EMBEDDER_OPENAI_URL=http://127.0.0.1:8399
"""
from __future__ import annotations

import argparse
import hashlib
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DIM = 768


def embed(text: str) -> list[float]:
    """Deterministic bag-of-tokens embedding; token -> dim by blake2b hash."""
    vec = [0.0] * DIM
    token = ""
    for ch in text.lower() + " ":
        if ch.isalnum():
            token += ch
            continue
        if token:
            h = hashlib.blake2b(token.encode(), digest_size=4).digest()
            vec[int.from_bytes(h, "big") % DIM] += 1.0
            token = ""
    norm = sum(v * v for v in vec) ** 0.5
    if norm > 0:
        return [v / norm for v in vec]
    # An all-zero row is degenerate and the index may reject it; pin an axis
    # no real token can collide with by construction is impossible here, so
    # pin dimension 0 and accept that empty strings cluster together.
    vec[0] = 1.0
    return vec


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args: object) -> None:
        # The job log is long enough; failures still surface as HTTP errors.
        pass

    def do_GET(self) -> None:  # noqa: N802 — BaseHTTPRequestHandler API
        # Readiness probe for the workflow's wait loop.
        self.send_response(200)
        self.send_header("content-type", "text/plain")
        self.send_header("content-length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def do_POST(self) -> None:  # noqa: N802 — BaseHTTPRequestHandler API
        length = int(self.headers.get("content-length", "0"))
        try:
            body = json.loads(self.rfile.read(length) or b"{}")
        except json.JSONDecodeError as e:
            self.send_error(400, f"bad JSON: {e}")
            return
        inputs = body.get("input", [])
        if isinstance(inputs, str):
            inputs = [inputs]
        payload = json.dumps(
            {
                "object": "list",
                "model": body.get("model", "stub"),
                "data": [
                    {"object": "embedding", "index": i, "embedding": embed(t)}
                    for i, t in enumerate(inputs)
                ],
            }
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--port", type=int, default=8399)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args()
    srv = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"stub embedder listening on http://{args.host}:{args.port}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
