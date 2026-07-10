# Lunaris Quickstart — Rust

Goal: from a fresh checkout, run your first Lunaris ingest + recall
against a local Postgres backend in under 10 minutes.

This example targets the **v0.2.x OSS Foundation** milestone — Postgres
+ pgvector as the canonical backend, no Moon required.

## Prerequisites

- `docker` + `docker compose` v2.20+
- `cargo` 1.94+ (Rust edition 2024 toolchain)
- the granite-r2 Q4_K_M GGUF staged at `~/.lunaris/models/` (default llama.cpp embedder); graph extraction optionally via a remote provider — see below

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
quickstart: recalled 1 hit(s) for "hello"
quickstart:   top hit score=0.83 text="# Hello from Lunaris …"
quickstart: DSL form returned 1 hit(s)
```

## Tear-down

```bash
docker compose down -v   # -v wipes the pg data volume
```

## In-process variant (no Ollama)

To run with all-Candle in-process (Gemma 3 4B extractor + EmbeddingGemma
embedder — remote-only since the llama.cpp cutover):

```bash
# Extraction/verification run against a remote provider (or any
# OpenAI-compatible server such as a local Ollama):
export LUNARIS_GRAPH_ENABLED=1
export LUNARIS_EXTRACT_PROVIDER=openai-compat
export LUNARIS_OPENAI_COMPAT_BASE_URL=http://127.0.0.1:11434/v1
export OPENAI_COMPAT_EXTRACT_MODEL=gemma3:4b
cargo run --release
```

See `docs/migration/0.5-to-0.6-llamacpp-only.md` for the full
provider matrix (anthropic | openai | gemini | minimax | openai-compat).

## Recall walkthrough

The binary recalls the episode it just ingested, scoped to the bound
`Scope`. Two equivalent forms:

```rust
// 1. The one-liner — default retrieval root is Vector::new("chunks", 30).
let hits = scoped.recall(lunaris::Query::text("hello")).await?;
println!("recalled {} hit(s); top score = {}", hits.len(), hits[0].score);

// 2. The DSL form — build the operator tree explicitly, cap at 5 hits.
use lunaris::Vector;
let hits = scoped
    .dsl()
    .with_root(Vector::new("chunks", 30).top(5))
    .execute(lunaris::Query::text("hello"))
    .await?;
```

`recall` returns `Vec<lunaris::Hit>`; each `Hit` carries `id`, `score`,
`text`, `source`, `heading_path`, `valid_from` / `valid_to`, plus the
`degraded` / `rerank_applied` / `source_op` provenance flags. See
`crates/lunaris-retrieve/src/types.rs`.

For the full DSL surface (BM25 keyword, graph traversal, RRF fusion,
cross-encoder rerank, `as_of` time-travel) see the retrieval DSL guide
at `docs.lunaris.dev`.

## Troubleshooting

- Vector recall returns empty rows / a `WARN` about the embedder →
  the granite-r2 Q4_K_M GGUF isn't staged at `~/.lunaris/models/`.
  Download it out-of-band (SHA-256s:
  `cargo run -p lunaris-bench --bin stage-models -- --help`) or set
  `LUNARIS_EMBEDDER_GGUF` to an existing copy.
- `postgres connection refused` → the container is still starting.
  Wait for the healthcheck (`docker compose ps`).
- `relation "episodes" does not exist` → migrations haven't run. Re-run
  step 3.

## What's next

- `examples/quickstart-py/` — same flow via `pip install lunaris`.
- `examples/quickstart-ts/` — same flow via `npm i lunaris`.
- `docs.lunaris.dev` — full concepts (bi-temporal, MVCC, atomic write,
  ACT-R consolidator).
