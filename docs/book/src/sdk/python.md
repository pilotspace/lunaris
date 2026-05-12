# Python SDK

**`pip install lunaris` gives you the same memory engine as the Rust crate,
behind a PyO3 0.26 binding generated from the same annotated surface** — so
`open / ingest / recall / forget / snapshot` and the composable retrieve DSL
behave identically across all three SDKs. `cargo run -p lunaris-codegen --
--check` gates every PR, so the surfaces never drift.

> Adapted from `docs/bindings.md` (Python half) and `docs/sdk/embedder-config.md`.

## Install

```bash
pip install lunaris
```

Prebuilt wheels ship for 5 targets (BIND-PY-05): `linux-x86_64`
(manylinux_2_28), `linux-aarch64` (manylinux_2_28), `macosx-x86_64`,
`macosx-arm64`, `win-amd64`. They use the `abi3-py311` stable ABI, so **one
wheel per target covers Python 3.11, 3.12, and 3.13**.

The bundled wheels are built with `default-features = false, features =
["ollama"]` — so `lunaris.open(...)` works without a local weights cache as
long as Ollama is reachable at first embed call. To use the in-process
fastembed/candle embedder instead, supply an `EmbedderConfig` (below) or
build from source.

No matching wheel? Source install (needs Rust 1.94+ and a maturin toolchain;
2–5 min):

```bash
pip install lunaris --no-binary lunaris
```

## Quickstart

```python
import asyncio
import lunaris
import ulid


async def main():
    handle = await lunaris.open("moon://127.0.0.1:6379")

    lsn = await handle.ingest({
        "id": str(ulid.ULID()),
        "source": "py-quickstart",
        "content": "Lunaris bi-temporal memory — hello from Python.",
        "metadata": {},
        "t_ref": None,
        "bt": {
            "valid": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
            "sys":   [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
        },
    })
    print("ingested at LSN", lsn)

    hits = await (
        lunaris.RetrievalBuilder()
            .bind(handle)
            .top(5)
            .execute()
    )
    for h in hits:
        print(h)


asyncio.run(main())
```

> `lunaris.RetrievalBuilder` is the pure-Python plan builder from
> `lunaris.dsl` (the package `__init__` re-exports it over the raw PyO3 class,
> whose builder methods are `NotImplementedError` stubs). `handle.recall()`
> returns one pre-bound to `handle`; a free `lunaris.RetrievalBuilder()` needs
> a `.bind(handle)` before `.execute()`.

Swap in `postgres://postgres:pass@localhost/lunaris` to run against Postgres
— the URL scheme is the only difference. See
[Choosing a Backend](../operations/backends.md).

## The wire shape

Episodes, forget requests, and hits cross the FFI as plain Python **dicts /
lists** (`pythonize` round-trips them to the Rust structs). For the bare
`handle.ingest(dict)` path you build the `bt` bi-temporal stamp and the ULID
`id` by hand, as in the quickstart.

For multi-agent partitioning the v0.2 surface (Wave 3G) also ships the typed
ergonomics:

```python
from lunaris import Scope, EpisodeBuilder

scoped = handle.scoped(Scope("acme.agent-1"))          # ScopedLunaris
lsn = await scoped.ingest(
    EpisodeBuilder("notes", "Lunaris ingest via the typed builder.")
        .metadata({"topic": "demo"})
)
hits = await scoped.recall("what did agent-1 note?")    # scope-pinned recall
```

`Scope("…")` validates against `[A-Za-z0-9_\-.]{1,128}` and raises
`ValueError` on a bad string, so "ingest into agent A, recall from agent B" is
a construction error rather than a silent leak. `EpisodeBuilder` mirrors the
Rust builder; its terminal `into_episode` is crate-private — only
`ScopedLunaris.ingest` may call it. (`Scope`, `EpisodeBuilder`,
`ScopedLunaris`, and `handle.scoped(...)` are exported from the package root.)

## The retrieval DSL via `RetrievalBuilder`

`Vector`, `Keyword`, `Graph` (from `lunaris.dsl`, re-exported at the package
root) compose via `.and_()`, `.fuse_rrf(k)`, `.top(n)`, `.filter(...)` /
`.filter_str(s)`, `.as_of(ms)`. A terminal `.execute()` collapses the whole
plan into a **single FFI call** — the plan is built in Python, executed once
in Rust:

```python
hits = await (
    handle.recall()                       # pre-bound RetrievalBuilder, default root Vector("chunks", 30)
        .and_(lunaris.Keyword.bm25("chunks", 30))
        .fuse_rrf(60)                      # Reciprocal Rank Fusion, k=60
        .top(5)
        .execute()
)
# hits is List[Hit dicts]; each carries content, source, score, raw_score,
# valid_time, sys_time, degraded (bool), rerank_applied (bool), source_op.
```

`.execute()` takes no arguments in the v0.2 Python DSL — the plan tree
collapses to the `index` / `k` / optional `filter` / `as_of_ms` knobs that
the `recall_simple_execute` FFI accepts; a query-text setter on the builder
is a follow-up. (Same shape on the TS side — see
[TypeScript SDK](./typescript.md).)

Time-travel is one combinator (`.as_of(wall_ms)` — milliseconds since the
Unix epoch):

```python
from datetime import datetime, timezone
snap_ms = int(datetime(2024, 6, 1, tzinfo=timezone.utc).timestamp() * 1000)
hits = await handle.recall().as_of(snap_ms).execute()
```

See [The Retrieval DSL](../guides/retrieval-dsl.md) for the full operator set.

## Pipeline toggles (three surfaces)

`GraphPipeline` and `ConsolidatorPipeline` default OFF. Flip them at code,
env, or config; resolution order is **code > env > config** — code wins.

```python
# code surface
handle.graph_pipeline.enable()
handle.consolidator_pipeline.disable()

# config surface — dict walked by the lunaris.open wrapper
handle = await lunaris.open(url, config={
    "graph_pipeline": {"enabled": True},
    "consolidator_pipeline": {"enabled": False},
})
```

```bash
# env surface — read at lunaris.open time
export LUNARIS_GRAPH_ENABLED=1
export LUNARIS_CONSOLIDATE_ENABLED=0
```

See [Consolidation & Verification](../guides/consolidate-verify.md) and
[The Graph Pipeline](../guides/graph.md).

## Embedder / reranker config

Override the env-driven default embedder/reranker from code via
`EmbedderConfig` / `RerankerConfig` (opaque handles wrapping a resolved
`Arc<dyn Embedder>`). Four `EmbedderConfig` factories:

| Factory | Use when |
|---|---|
| `EmbedderConfig.fastembed(cache_dir=…, execution="cpu"\|"coreml"\|"cuda", show_download_progress=False)` | The common case — ONNX EmbeddingGemma-300M (768-d), auto-downloaded (~600 MB, one-time per host). |
| `EmbedderConfig.ollama(endpoint="http://localhost:11434", model="embeddinggemma:300m", dim=768)` | A local Ollama server already running the model — no in-process ONNX load. |
| `EmbedderConfig.from_onnx_bytes(onnx_bytes=…, tokenizer_bytes=…, dim=…, pooling="mean"\|"cls", execution=…)` | BYO ONNX model already in memory (S3, secret store, …). |
| `EmbedderConfig.from_onnx_path(onnx_path=…, tokenizer_path=…, dim=…, pooling=…, execution=…)` | BYO ONNX model on disk (mounted model volume). |

```python
from lunaris import EmbedderConfig, RerankerConfig

emb = EmbedderConfig.fastembed(cache_dir="/var/cache/lunaris/fastembed", execution="coreml")
rer = RerankerConfig.fastembed(cache_dir="/var/cache/lunaris/fastembed-reranker")
handle = await lunaris.open("moon://127.0.0.1:6379", embedder=emb, reranker=rer)

# Latency floor — disable the cross-encoder rescoring pass:
handle = await lunaris.open(url, embedder=emb, reranker=RerankerConfig.noop())
```

`RerankerConfig` ships `fastembed()` (BGE-Reranker-v2-m3) and `noop()` today;
BYO ONNX for the reranker is deferred (the Rust wrapper doesn't plumb it yet).
Notes:

- The fastembed presets fetch weights from HF Hub on first use — point
  `cache_dir` (or `LUNARIS_FASTEMBED_CACHE_DIR`) at a writable path with
  ~1 GB free; the download is one-time per host.
- BYO ONNX models **must** match the declared `dim` — the SDK wraps them in
  `DimValidatingEmbedder`, which raises a `LunarisError` ("declared dim X does
  not match observed dim Y …") on the first batch if they don't, rather than
  silently corrupting your vector index.
- `execution="coreml"` / `"cuda"` need the wheel built with
  `lunaris-embed/fastembed-coreml` / `-cuda`; without it Python raises
  `ValueError` at config time. Python rejects unknown `execution` values
  strictly (a REPL-friendly failure mode).
- **FFI cliff:** you cannot implement the Rust `Embedder` / `Reranker` trait
  from Python — per-call FFI callbacks would be too slow for the hot path.
  Roll-your-own backends are a Rust-crate-only escape hatch; if your backend
  doesn't fit the preset / Ollama / BYO-ONNX shape, contribute a constructor
  to `lunaris-embed`.

## GIL / async notes

Every `.await` in the binding sits inside a
`pyo3_async_runtimes::tokio::future_into_py` closure — the GIL is released
across awaits (CLAUDE.md mandate; brace-balanced scan test in
`lunaris-codegen/tests/emitter_shape.rs`, end-to-end proof in
`crates/lunaris-py/tests/test_gil_discipline.py`). So:

- `await handle.ingest(...)` / `await handle.recall()...execute()` are real
  `asyncio` awaitables — use them inside an event loop (`asyncio.run`, an
  ASGI handler, etc.), not from synchronous code.
- A long ingest in one task does not block other Python threads — the GIL is
  not held while Lunaris is in Rust.
- The `lunaris` extension module is not part of `cargo test --workspace`
  (it's a `cdylib` that fails to link under the workspace test runner) — test
  Python code with `maturin develop` + `pytest`, or via
  `scripts/sdk-real-evidence.sh`.

## Troubleshooting

- **"No matching distribution found for lunaris"** — pip can't find a wheel
  for your Python ABI / platform. Check the target triple
  (`python -c "import sysconfig; print(sysconfig.get_platform())"`); if it
  isn't one of the 5 above, do a source install
  (`pip install lunaris --no-binary lunaris`, needs Rust 1.94+).
- **"embedding-gemma weights missing at …"** — the in-process candle/fastembed
  embedder needs weights. Either download them
  (`huggingface-cli download google/embeddinggemma-300m --local-dir
  ~/.cache/lunaris/models/embedding-gemma-300m/`) or use the Ollama embedder
  (the default for the bundled wheel — Ollama just has to be reachable).
- **`fastembed: failed to fetch model …`** — first-call download is hitting
  the network; pre-populate `cache_dir`, ensure write access + ~1 GB free,
  confirm outbound HTTPS to `huggingface.co`.
- **`conformance_fixture_episodes` not exported** — correct; that helper
  lives behind the `bindings-it` Cargo feature, used only by the per-driver
  parity tests. Production wheels ship without it.

## See also

- [TypeScript SDK](./typescript.md) — the parallel surface
- [The Retrieval DSL](../guides/retrieval-dsl.md)
- [Configuration Reference](../reference/configuration.md) — env vars / feature flags
- `crates/lunaris-py/` — the binding crate
