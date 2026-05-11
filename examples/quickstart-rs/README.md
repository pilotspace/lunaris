# Lunaris Quickstart — Rust

Goal: from a fresh checkout, run your first Lunaris ingest + recall
against a local Postgres backend in under 10 minutes.

This example targets the **v0.2.x OSS Foundation** milestone — Postgres
+ pgvector as the canonical backend, no Moon required.

## Prerequisites

- `docker` + `docker compose` v2.20+
- `cargo` 1.94+ (Rust edition 2024 toolchain)
- `ollama` 0.3+ running locally (or rebuild with `candle` — see below)

## Five steps

```bash
# 1. From the repo root
cd examples/quickstart-rs

# 2. Bring up Postgres + pgvector + pgmq + AGE
docker compose up -d
docker compose ps        # wait until `lunaris-quickstart-pg` is "healthy"

# 3. Apply migrations from the repo's storage crate (Phase 23: bake
#    these into the image so step 3 disappears)
sqlx migrate run --source ../../crates/lunaris-storage-postgres/migrations \
                 --database-url postgres://lunaris:lunaris@localhost:5432/lunaris

# 4. Start Ollama in the background and pull a tiny embedder
ollama serve &
ollama pull nomic-embed-text

# 5. Point the binary at Postgres and run
export LUNARIS_PG_URL="postgres://lunaris:lunaris@localhost:5432/lunaris"
cargo run --release
```

Expected output:

```
quickstart: opening lunaris handle at postgres://lunaris:lunaris@localhost:5432/lunaris
quickstart: ingested episode at lsn=Lsn(1) under scope `quickstart`
quickstart: ingest path verified; see README for recall walkthrough
```

## Tear-down

```bash
docker compose down -v   # -v wipes the pg data volume
```

## In-process variant (no Ollama)

To run with all-Candle in-process (Gemma 3 4B extractor + EmbeddingGemma
embedder, ~5 GB of weights, ~8 GB RAM at runtime):

```bash
# In examples/quickstart-rs/Cargo.toml, change the lunaris dep to:
#   lunaris = { path = "../../crates/lunaris", default-features = false, features = ["candle"] }
cargo run --release
```

For the **laptop-floor** path (RFC 0006 — Gemma 3 270M verifier, ~540 MB),
add `features = ["candle", "verify-small"]`. Note: 270M is a scaffold
in v0.2.x; the production default-flip is gated on Phase 24 bench.

## Recall walkthrough (Phase 23 follow-up)

The current binary verifies the ingest contract. The full recall
walkthrough (semantic search via the retrieval DSL, BM25 keyword,
graph traversal) lands as `examples/quickstart-rs/src/recall.rs`
once the v0.2.x DSL stabilizes for OSS adoption.

## Troubleshooting

- `Lunaris::open` returns `embedding-gemma weights missing` → the
  `candle` feature is on and weights aren't cached. Either pre-download
  via `huggingface-cli download google/embeddinggemma-300m --local-dir
  ~/.cache/lunaris/models/embeddinggemma-300m/` or rebuild with the
  `ollama` feature.
- `postgres connection refused` → the container is still starting.
  Wait for the healthcheck (`docker compose ps`).
- `relation "episodes" does not exist` → migrations haven't run. Re-run
  step 3.

## What's next

- `examples/quickstart-py/` — same flow via `pip install lunaris`.
- `examples/quickstart-ts/` — same flow via `npm i lunaris`.
- `docs.lunaris.dev` — full concepts (bi-temporal, MVCC, atomic write,
  ACT-R consolidator).
