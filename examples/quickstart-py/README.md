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

## Recall walkthrough (Phase 23 follow-up)

The typed `Scope` + `EpisodeBuilder` Python surface lands in v0.3 as
part of the multi-tenant SDK story. Today the Python wire-shape is a
dict (mirrors the `lunaris_core::primitives::Episode` serde form). The
upgrade path is a one-liner — `Scope("quickstart")` swaps in once
exposed.

For the recall side, the DSL is already exposed:

```python
hits = await handle.recall().vector("hello", top_k=3).execute()
```

A full recall walkthrough lands alongside `examples/quickstart-rs/`'s
recall example once the v0.2.x DSL stabilises for OSS.

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
