# Lunaris Quickstart — Python

Mirrors the [Rust quickstart](../quickstart-rs/README.md) against the
same Postgres backend. Demonstrates `lunaris.open` → `handle.ingest` →
`handle.recall` from a single Python script.

## Prerequisites

- `docker` + `docker compose` v2.20+
- Python 3.11+ with `pip` or `uv`
- `ollama` 0.3+ running locally

## Five steps

```bash
# 1. From the repo root
cd examples/quickstart-py

# 2. Reuse the Rust quickstart's Postgres image
docker compose -f ../quickstart-rs/docker-compose.yml up -d

# 3. Apply migrations
sqlx migrate run --source ../../crates/lunaris-storage-postgres/migrations \
                 --database-url postgres://lunaris:lunaris@localhost:5432/lunaris

# 4. Install lunaris + python-ulid
pip install lunaris python-ulid     # or: uv add lunaris python-ulid

# 5. Point at Postgres and run
ollama serve &  ollama pull nomic-embed-text
export LUNARIS_PG_URL="postgres://lunaris:lunaris@localhost:5432/lunaris"
python quickstart.py
```

Expected output:

```
quickstart: opening lunaris handle at postgres://...
quickstart: ingested episode at lsn=...:... under scope `quickstart`
quickstart: ingest path verified; see README for recall walkthrough
```

## Local-dev variant (no PyPI release yet)

If you're developing against this repo before the PyPI release:

```bash
cd ../../crates/lunaris-py
maturin develop --release
cd ../../examples/quickstart-py
python quickstart.py
```

`maturin develop` compiles the lunaris-py cdylib in-place and installs
it into the active virtualenv. The script then imports the local build.

## Recall — current binding limitations (v0.2.x)

The Rust quickstart ([../quickstart-rs/](../quickstart-rs/)) does a
real scoped recall. The Python binding is **not there yet** — two gaps,
both v0.3 deliverables:

1. **No scope-aware recall.** `handle.recall().…​.execute()` exists, but
   it always queries the default `_dev_` partition — there's no scope
   parameter on the Python builder, so it can't see the `quickstart`
   scope this script ingests into. (The Rust path threads a real `Scope`
   all the way through ingest *and* recall.)
2. **No query-text parameter.** The Python DSL builder (`Vector(index,
   k)`, `.top(n)`, `.fuse_rrf(k)`, `.as_of(ms)`, `.filter(...)`)
   collapses to a plan whose `query` field is always the empty string —
   the FFI bridge `recall_simple_execute` accepts the
   `Vector("chunks", k)` default shape only (see
   `crates/lunaris-py/src/dsl.rs`). So `handle.recall().execute()`
   returns hits for an *empty* query, not a useful one. Passing a real
   query string lands when the FFI surface is widened in v0.3.

The typed `Scope` + `EpisodeBuilder` Python surface also lands in v0.3;
today the wire shape is a dict (mirrors `lunaris_core::primitives::Episode`).

Until then, this quickstart stops at the ingest contract. Follow the
Rust quickstart for the end-to-end recall walkthrough.

## Troubleshooting

- `ImportError: No module named 'lunaris'` → run `pip install lunaris`
  (or `maturin develop` for local dev).
- `lunaris.LunarisError: embedding-gemma weights missing` → the
  default features built a candle-backed embedder and the weights
  aren't cached. Rebuild the wheel without the `candle` feature
  (Ollama-only path) or pre-download the weights.
- `postgres connection refused` → wait for the docker-compose
  healthcheck (`docker compose ps -f ../quickstart-rs/docker-compose.yml`).
- `relation "episodes" does not exist` → re-run step 3.
