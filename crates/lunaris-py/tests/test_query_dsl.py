"""W0.5 — the documented Python recall chain must be able to express a query.

Written from the CONSUMER's side: every test here calls the exact chain the
README quickstart shows. Before the fix, `RetrievalBuilder` had no `.query()`
at all and `_collapse_plan` hardcoded `{"query": ""}` — so the SDK's only
documented recall path searched for the empty string.

Three tiers, cheapest first:

1. `test_query_reaches_the_collapsed_plan` — offline, no backend, no
   inference. The single discriminating assertion for the wiring bug.
2. `test_top_survives_the_collapse` — offline. `.top(n)` was silently
   overwritten by the leaf operator's `k` because `_collapse_plan` visited
   parents before children.
3. `test_readme_python_sample_returns_real_hits` — live Moon + a stub
   OpenAI-compatible embedder that doubles as a SPY. Proves the query text
   crosses the FFI into the Rust engine and that a real query outranks an
   unrelated document. No llama.cpp, no GGUF, fully deterministic.

The stub embedder needs the wheel built with `--features embed-remote`
(`maturin build --no-default-features --features embed-remote`); tier 3
skips loudly otherwise.
"""
from __future__ import annotations

import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import lunaris
from lunaris import RetrievalBuilder, Vector
from lunaris.dsl import _collapse_plan

# --------------------------------------------------------------------------
# Tier 1 + 2 — offline plan-collapse assertions.
# --------------------------------------------------------------------------


def test_query_reaches_the_collapsed_plan() -> None:
    """The README chain must put the caller's text in the plan's `query`."""
    builder = RetrievalBuilder().query("who likes chocolate?").top(3)
    plan = _collapse_plan(builder._node)
    assert plan["query"] == "who likes chocolate?", (
        "the DSL dropped the query text on the floor; recall would search "
        f"for {plan['query']!r}"
    )


def test_query_composes_with_the_other_operators() -> None:
    """`.query()` chains like `and_` / `fuse_rrf` / `top` / `filter` / `as_of`."""
    builder = (
        Vector("chunks", 30)
        .query("chocolate")
        .filter(source="quickstart")
        .as_of(1_000_000)
        .top(4)
    )
    assert isinstance(builder, RetrievalBuilder)
    plan = _collapse_plan(builder._node)
    assert plan["query"] == "chocolate"
    assert plan["index"] == "chunks"
    assert plan["k"] == 4
    assert plan["filter"] == "source = 'quickstart'"
    assert plan["as_of_ms"] == 1_000_000


def test_top_survives_the_collapse() -> None:
    """`.top(n)` must win over the leaf operator's `k` — it is the outer op."""
    plan = _collapse_plan(Vector("chunks", 30).top(5)._node)
    assert plan["k"] == 5, f"`.top(5)` was overwritten by the leaf k: {plan['k']}"


def test_query_is_bound_to_the_handle_through_the_chain() -> None:
    """Chaining off a bound builder keeps the handle (else `.execute()` throws)."""
    sentinel = object()
    chained = RetrievalBuilder(handle=sentinel).query("anything").top(2)
    assert chained._handle is sentinel


# --------------------------------------------------------------------------
# Tier 3 — live Moon + deterministic stub embedder (also a spy).
# --------------------------------------------------------------------------

_DIM = 768  # lunaris_embed_remote::openai::DEFAULT_DIM


def _embed(text: str) -> list[float]:
    """Deterministic bag-of-tokens embedding. Token -> dim by blake2b hash.

    Overlapping vocabulary produces a positive cosine; disjoint vocabulary
    produces 0. That makes ranking assertions exact instead of probabilistic.
    """
    vec = [0.0] * _DIM
    token = ""
    for ch in text.lower() + " ":
        if ch.isalnum():
            token += ch
            continue
        if token:
            h = hashlib.blake2b(token.encode(), digest_size=4).digest()
            vec[int.from_bytes(h, "big") % _DIM] += 1.0
            token = ""
    norm = sum(v * v for v in vec) ** 0.5
    if norm > 0:
        vec = [v / norm for v in vec]
    else:
        # An all-zero row would be rejected/degenerate; pin a constant axis.
        vec[0] = 1.0
    return vec


class _StubEmbedder:
    """OpenAI-compatible `/v1/embeddings` server that records every input."""

    def __init__(self) -> None:
        self.seen: list[str] = []
        self._lock = threading.Lock()
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args: object) -> None:  # silence stderr
                pass

            def do_POST(self) -> None:  # noqa: N802 — BaseHTTPRequestHandler API
                length = int(self.headers.get("content-length", "0"))
                body = json.loads(self.rfile.read(length) or b"{}")
                inputs = body.get("input", [])
                if isinstance(inputs, str):
                    inputs = [inputs]
                with outer._lock:
                    outer.seen.extend(inputs)
                payload = json.dumps(
                    {
                        "object": "list",
                        "model": body.get("model", "stub"),
                        "data": [
                            {"object": "embedding", "index": i, "embedding": _embed(t)}
                            for i, t in enumerate(inputs)
                        ],
                    }
                ).encode()
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self._srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._srv.serve_forever, daemon=True)

    @property
    def base_url(self) -> str:
        host, port = self._srv.server_address[:2]
        return f"http://{host}:{port}"

    def __enter__(self) -> "_StubEmbedder":
        self._thread.start()
        return self

    def __exit__(self, *_exc: object) -> None:
        self._srv.shutdown()
        self._srv.server_close()


@pytest.fixture
def stub_embedder():
    """Point Lunaris at the stub embedder for the duration of one test."""
    with _StubEmbedder() as stub:
        prev = os.environ.get("LUNARIS_EMBEDDER_OPENAI_URL")
        os.environ["LUNARIS_EMBEDDER_OPENAI_URL"] = stub.base_url
        try:
            yield stub
        finally:
            if prev is None:
                os.environ.pop("LUNARIS_EMBEDDER_OPENAI_URL", None)
            else:
                os.environ["LUNARIS_EMBEDDER_OPENAI_URL"] = prev


def _episode(content: str) -> dict:
    import ulid

    return {
        "id": str(ulid.ULID()),
        "scope": "_dev_",
        "source": "quickstart",
        "content": content,
        "t_ref": None,
        "bt": {
            "valid": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
            "sys": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
        },
        "metadata": {},
    }


@pytest.mark.asyncio
async def test_readme_python_sample_returns_real_hits(
    moon_backend_url: str, stub_embedder: _StubEmbedder
) -> None:
    """The exact chain the README quickstart shows, end to end.

    `handle.recall().query(...).top(...).execute()` must (a) reach real rows
    and (b) rank the document that answers the query first.
    """
    handle = await lunaris.open(moon_backend_url)
    await handle.ingest(_episode("Alice loves chocolate."))
    await handle.ingest(_episode("Bob repairs bicycles."))

    if not stub_embedder.seen:
        pytest.skip(
            "the stub embedder was never called — this wheel was built "
            "without `embed-remote`; rebuild with `maturin build "
            "--no-default-features --features embed-remote` to run the live "
            "query assertion"
        )

    hits = await handle.recall().query("chocolate").top(5).execute()

    assert hits, "the documented query chain returned no hits"
    assert "chocolate" in hits[0]["text"].lower(), (
        f"a real query did not rank its answer first: {hits[0]['text']!r}"
    )
    # The spy proves the text crossed the FFI rather than being collapsed away.
    assert "chocolate" in stub_embedder.seen, (
        "the query text never reached the engine; the embedder saw "
        f"{stub_embedder.seen!r}"
    )


# --------------------------------------------------------------------------
# The README sample itself — extracted and RUN, not paraphrased.
# --------------------------------------------------------------------------

_REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]


def _readme_python_block() -> str:
    md = (_REPO_ROOT / "README.md").read_text()
    m = re.search(r"```python\n(.*?)```", md, re.S)
    assert m, "no ```python block in README.md"
    return m.group(1)


def test_readme_python_sample_runs_verbatim(
    moon_backend_url: str, stub_embedder: _StubEmbedder
) -> None:
    """Execute the README's Python quickstart as written.

    One substitution only — the hardcoded dev Moon URL becomes the test Moon
    (6399; 6379/6380/6381 are off limits). Everything else, including the
    recall chain, runs exactly as a reader would copy it.
    """
    src = _readme_python_block()
    runnable = src.replace('"moon://127.0.0.1:6380"', repr(moon_backend_url))
    assert runnable != src, "README quickstart no longer opens moon://127.0.0.1:6380"

    env = dict(os.environ)
    env["LUNARIS_EMBEDDER_OPENAI_URL"] = stub_embedder.base_url
    proc = subprocess.run(
        [sys.executable, "-c", runnable],
        capture_output=True,
        text=True,
        timeout=180,
        env=env,
    )
    assert proc.returncode == 0, (
        f"the README quickstart does not run:\n--- stderr ---\n{proc.stderr}"
    )
    assert "chocolate" in proc.stdout.lower(), (
        f"the README quickstart printed no matching hit: {proc.stdout!r}"
    )


# --------------------------------------------------------------------------
# The PyO3-frozen `RetrievalBuilder` trap — pinned, deliberately, not fixed.
# --------------------------------------------------------------------------
#
# `crates/lunaris-py/src/generated.rs` is emitted by `lunaris-codegen`
# (`cargo run -p lunaris-codegen -- --emit py`) and a CI parity-check job
# fails the PR on any hand-edit, so those stub bodies CANNOT be fixed from
# this crate — the fix belongs in `crates/lunaris-codegen/src/emit_py.rs`,
# which owns the `PyNotImplementedError` string.
#
# What keeps the trap dead today is `lunaris/__init__.py`, which rebinds the
# public name `lunaris.RetrievalBuilder` to the pure-Python builder, plus
# `_attach_recall_shim`, which rebinds `handle.recall`. Both are load-bearing
# and neither is obvious. These two tests say so out loud and fail the moment
# somebody removes either shim.


def test_public_retrieval_builder_shadows_the_frozen_pyo3_stub() -> None:
    """`lunaris.RetrievalBuilder` must be the working Python class."""
    from lunaris import lunaris as _native  # the compiled cdylib

    assert lunaris.RetrievalBuilder is not _native.RetrievalBuilder, (
        "lunaris.RetrievalBuilder is the PyO3-frozen stub whose every builder "
        "method raises NotImplementedError — the __init__.py rebinding is gone"
    )
    assert hasattr(lunaris.RetrievalBuilder, "query")


@pytest.mark.asyncio
async def test_handle_recall_returns_the_working_builder(moon_backend_url: str) -> None:
    """`handle.recall()` must be the shim, not the frozen PyO3 method.

    Calling the frozen class method directly documents the trap: it hands
    back a `RetrievalBuilder` whose `.top()` / `.and()` / `.fuse_rrf()` /
    `.filter()` / `.as_of()` all raise `NotImplementedError`.
    """
    from lunaris import lunaris as _native

    handle = await lunaris.open(moon_backend_url)
    assert isinstance(handle.recall(), lunaris.RetrievalBuilder)

    frozen = type(handle).recall(handle)
    assert isinstance(frozen, _native.RetrievalBuilder)
    with pytest.raises(NotImplementedError) as exc:
        frozen.top(5)
    assert "lunaris.dsl" in str(exc.value)
