# lunaris

PyO3 0.26 Python bindings for the [Lunaris](https://github.com/lunaris-dev/lunaris) agent memory engine.

## Installation

From source (development):

```bash
cd crates/lunaris-py
uv venv --python 3.11 .venv
source .venv/bin/activate
uv pip install maturin
maturin develop --release
```

A PyPI wheel will ship with the v0.1.1 release via the Plan 08-04 multi-platform prebuild matrix.

For the full user-facing install + quickstart guide covering both Python and TypeScript, see [`docs/bindings.md`](../../docs/bindings.md).

## Requirements

- Python **3.11+** (abi3 stable ABI — one wheel covers 3.11, 3.12, 3.13)
- A Moon or Postgres backend reachable from the process; `moon://` and `postgres://` URL schemes are supported.

## Example

```python
import asyncio
import lunaris

async def main():
    handle = await lunaris.open("moon://127.0.0.1:6379")
    lsn = await handle.ingest({
        "id": "01JABCDEFGHJKMNPQRSTVWXYZ0",
        "source": "py-example",
        "content": "Lunaris bi-temporal hello.",
        "metadata": {},
        "t_ref": None,
        "bt": {"valid": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
               "sys":   [{"wall_ms": 0, "counter": 0, "node_id": 0}, None]},
    })
    print("ingested at", lsn)

    hits = await handle.recall().execute()
    for h in hits:
        print(h)

asyncio.run(main())
```

## Custom embedder + reranker

Pick a preset or bring your own ONNX model. The `EmbedderConfig` and
`RerankerConfig` factories swap the backend at handle-construction time;
the env-driven default remains in place for callers that don't pass one.

```python
import asyncio
import lunaris
from lunaris import EmbedderConfig, RerankerConfig

async def main():
    cfg = EmbedderConfig.fastembed(cache_dir="/var/cache/lunaris/fastembed")
    handle = await lunaris.open(
        "moon://127.0.0.1:6379",
        embedder=cfg,
        reranker=RerankerConfig.noop(),   # disable cross-encoder rerank
    )
    # ... ingest / recall as usual

asyncio.run(main())
```

See [`docs/sdk/embedder-config.md`](../../docs/sdk/embedder-config.md) for
the full customization guide — preset fastembed, preset Ollama, BYO ONNX
bytes, and BYO ONNX path — with troubleshooting and the FFI-cliff limits.

## Surface parity

The Python class / method surface is generated from `crates/lunaris-codegen/annotations/surface.toml` (Plan 08-01). The parity-check CI job fails any PR that drifts the committed snapshot from the regenerated output — `pip install lunaris` never lags the Rust crate.

## Three-surface pipeline toggles

The `GraphPipeline` and `ConsolidatorPipeline` default to OFF (blueprint §5.1 / §5.2). Flip them at any of three surfaces:

| Surface | Example |
| ------- | ------- |
| Code    | `handle.graph_pipeline.enable()` |
| Env     | `LUNARIS_GRAPH_ENABLED=1 python run.py` |
| Config  | `await lunaris.open(url, config={"graph_pipeline": {"enabled": True}})` |

Resolution order: code > env > config — code is always authoritative.
