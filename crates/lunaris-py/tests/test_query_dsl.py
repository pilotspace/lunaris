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
import uuid
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import lunaris
from lunaris import Graph, Keyword, RetrievalBuilder, Vector
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
    assert plan["filter"] == "source = 'quickstart'"
    assert plan["as_of_ms"] == 1_000_000
    assert plan["root"] == {
        "op": "top",
        "n": 4,
        "child": {"op": "vector", "index": "chunks", "k": 30},
    }


def test_top_stays_the_outer_operator() -> None:
    """`.top(n)` is the outer op and must stay outer.

    Pre-F14 the flat plan had ONE `k` for both `.top(n)` and the leg's own
    `k`, so they fought over it and a parents-before-children walk let the
    leaf win. F14 gives each its own node, which is what the Rust DSL means:
    fetch 30 candidates, return the best 5.
    """
    plan = _collapse_plan(Vector("chunks", 30).top(5)._node)
    assert plan["root"] == {
        "op": "top",
        "n": 5,
        "child": {"op": "vector", "index": "chunks", "k": 30},
    }


def test_a_second_retrieval_leg_is_carried_not_dropped() -> None:
    """Pre-F14 the plan held ONE index and ONE k, so this test asserted a
    REFUSAL: `Vector("chunks", 10).and_(Keyword.bm25("facts", 20))` collapsed
    to `{"index": "facts", "k": 5}` — a single-leg query whose index was
    decided by the order the operands happened to be written in, and flipping
    them searched "chunks" instead.

    F14 carries the tree across the FFI, so the composition survives WITH its
    operand order — the property the single-leg collapse could not preserve.
    """
    plan = Vector("chunks", 10).and_(Keyword.bm25("facts", 20)).fuse_rrf(60).top(5)
    assert _collapse_plan(plan._node)["root"] == {
        "op": "top",
        "n": 5,
        "child": {
            "op": "fuse_rrf",
            "k": 60,
            "child": {
                "op": "and",
                "left": {"op": "vector", "index": "chunks", "k": 10},
                "right": {"op": "keyword", "index": "facts", "k": 20},
            },
        },
    }

    flipped = Keyword.bm25("facts", 20).and_(Vector("chunks", 10))
    assert _collapse_plan(flipped._node)["root"]["left"] == {
        "op": "keyword",
        "index": "facts",
        "k": 20,
    }


def test_a_graph_leg_is_carried_and_a_bare_name_is_not_an_anchor() -> None:
    """`docs/MIGRATING-FROM-ZEP.md` sells graph traversal as a reason to move
    to Lunaris, and pre-F14 a `graph` leg had no field in the flat FFI, so it
    vanished. It now reaches the engine.

    A bare `"alice"` still raises: an EntityId is derived from `(name, type)`,
    so a bare name needs a guessed type, and the wrong type anchors on an
    entity that does not exist — which returns empty, exactly like a real
    absence of edges.
    """
    plan = (
        Vector("chunks", 30)
        .and_(Graph.anchored([("Alice", "Person")], hops=2))
        .top(5)
    )
    assert _collapse_plan(plan._node)["root"]["child"]["right"] == {
        "op": "graph",
        "seeds": [{"name": "Alice", "type": "Person"}],
        "hops": 2,
    }

    bare = Vector("chunks", 30).and_(Graph.anchored(["alice"], hops=2)).top(5)
    with pytest.raises(NotImplementedError) as excinfo:
        _collapse_plan(bare._node)
    msg = str(excinfo.value)
    assert "name" in msg and "type" in msg, msg


def test_the_hex_seed_form_accepts_exactly_what_the_typescript_twin_accepts() -> None:
    """Both SDKs document one hex spelling: 32 chars, `[0-9a-f]` only.

    `int(seed, 16)` is the obvious Python check and the wrong one — it accepts
    underscores (`int("1_2", 16) == 18`) and surrounding whitespace, so a seed
    Python waved through would be rejected by the TypeScript twin's
    `/^[0-9a-f]{32}$/i`. Two SDKs accepting different inputs for the same
    documented shape is the parity bug the marshallers exist to avoid.
    """
    ok = "0123456789abcdef0123456789abcdef"
    assert _collapse_plan(Graph.anchored([ok], hops=1)._node)["root"]["seeds"] == [ok]

    for bad in (
        "0123456789abcdef0123456789abc_ef",  # int(_, 16) accepts underscores
        " 123456789abcdef0123456789abcdef",  # ...and leading whitespace
        "0123456789abcdef0123456789abcde",   # 31 chars
        "0123456789abcdef0123456789abcdefa",  # 33 chars
        "0123456789abcdef0123456789abcdeg",  # not hex
    ):
        with pytest.raises(NotImplementedError):
            _collapse_plan(Graph.anchored([bad], hops=1)._node)


def test_an_operator_with_no_marshalling_is_refused_not_silently_dropped() -> None:
    """The DSL cannot build such a node today, so this reaches past the public
    surface to prove the fallthrough is a raise. Without it, adding a
    combinator to the DSL and forgetting the marshalling arm would silently
    drop the operator — the exact defect F14 removed."""
    from lunaris.dsl import _OpNode

    with pytest.raises(NotImplementedError) as excinfo:
        _collapse_plan(_OpNode("raptor_descend", (3,)))
    assert "raptor_descend" in str(excinfo.value)


def test_a_no_op_operator_is_not_an_error() -> None:
    """The rule is "refuse what would change the plan", not "refuse anything
    unusual". `fuse_rrf` over a single leg fuses nothing, and a no-op is not
    a lie — it is carried through as written."""
    plan = _collapse_plan(Vector("chunks", 30).fuse_rrf(60).top(5)._node)
    assert plan["root"] == {
        "op": "top",
        "n": 5,
        "child": {
            "op": "fuse_rrf",
            "k": 60,
            "child": {"op": "vector", "index": "chunks", "k": 30},
        },
    }


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


def _episode_in(scope: str, content: str) -> dict:
    """`_episode` with a caller-chosen partition key.

    W4.12's test needs a FOREIGN row present to prove the read is partitioned;
    a row only in the scope under test cannot distinguish isolation from an
    empty store.
    """
    ep = _episode(content)
    ep["scope"] = scope
    return ep


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


# W4.12 — a DSL query must be bindable to a partition.
#
# `handle.recall()` hands out the working `lunaris.dsl.RetrievalBuilder`
# (`_attach_recall_shim`), but `handle.scoped(s).dsl()` returns the
# codegen-frozen NATIVE builder instead, whose whole surface is
# `['and', 'as_of', 'filter', 'fuse_rrf', 'top']` — no terminal op at all. So
# the only scope-bound DSL entry point in the SDK cannot be executed, and
# `dsl.py` has no scope support by any other route either. For a product whose
# core claim includes multi-agent isolation, that is a hole in the public
# surface.
#
# The control below is the point of the test, not ceremony: it proves the
# fixture partitions rows at all, so a failure downstream is the DSL path and
# not the ingest. Isolation is a property of the READ — asserting it needs a
# foreign row present that the read must not return.
@pytest.mark.asyncio
async def test_scoped_dsl_binds_the_partition(moon_backend_url: str) -> None:
    tag = uuid.uuid4().hex[:10]
    a = lunaris.Scope(f"w412a-{tag}")
    b = lunaris.Scope(f"w412b-{tag}")

    handle = await lunaris.open(moon_backend_url)
    # Tagged so a previous run's rows — which live in a DIFFERENT scope but
    # the SAME store — cannot satisfy either assertion below.
    a_text = f"Alice loves chocolate cake ({tag})."
    b_text = f"Bob also loves chocolate cake ({tag})."
    await handle.ingest(_episode_in(str(a), a_text))
    await handle.ingest(_episode_in(str(b), b_text))

    # CONTROL — the non-DSL scoped path. If this cannot see A's row, no
    # embedder produced usable vectors and the assertion below would be
    # measuring the wrong thing.
    control = await handle.scoped(a).recall("chocolate")
    control_texts = [h.get("text", "") for h in control]
    if not control_texts:
        pytest.skip(
            "scoped(a).recall returned nothing — no usable embedder in this "
            "build, so the scoped-DSL assertion has no control to stand on"
        )
    assert b_text not in control_texts, (
        f"the CONTROL leaked the other scope's row: {control_texts}"
    )

    # THE ASSERTION — same partition, composed through the DSL.
    hits = await handle.scoped(a).dsl().query("chocolate cake").top(5).execute()
    texts = [h.get("text", "") for h in hits]
    assert texts, "scoped(a).dsl() … .execute() returned nothing"
    # Both halves are load-bearing, and they fail on DIFFERENT defects.
    #
    # POSITIVE: an unbound plan runs at `Scope::dev()`, a partition this test
    # never wrote to — so it comes back with other tests' leftovers rather than
    # empty, and an exclusion-only assertion passes while reading the wrong
    # tenant entirely. Only a plan actually bound to `a` can contain a_text.
    assert a_text in texts, (
        f"scoped(a).dsl() did not return scope a's own row — the plan reached "
        f"the engine unbound and read some other partition: {texts}"
    )
    # NEGATIVE: b_text is deliberately near-identical to a_text, so it outranks
    # everything else in the store for this query. If the scope were carried
    # but ignored on the read path, it would be right here.
    assert b_text not in texts, (
        f"scoped(a).dsl() escaped its partition and returned scope b's row: {texts}"
    )
