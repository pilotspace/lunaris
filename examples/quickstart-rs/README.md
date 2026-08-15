# Lunaris Quickstart — Rust

Goal: from a fresh checkout, run your first Lunaris ingest + recall
against a local Moon in under 10 minutes.

**0.7.0 is Moon-only.** This quickstart targeted Postgres + pgvector
through 0.6.x; that backend, the SQLite one, and `lunaris-migrate` were
all deleted. If you have 0.6.x data to bring across, run `lunaris-migrate`
from the **v0.6.2 release binary before upgrading** — see
[`docs/migration/0.6-to-0.7.md`](../../docs/migration/0.6-to-0.7.md).

## Prerequisites

- `docker` + `docker compose` v2.20+
- `cargo` 1.94+ (Rust edition 2024 toolchain)
- the granite-r2 Q4_K_M GGUF staged at `~/.lunaris/models/` (default llama.cpp embedder); graph extraction optionally via a remote provider — see below

## Four steps

```bash
# 1. From the repo root
cd examples/quickstart-rs

# 2. Bring up Moon (single shard — see the compose file for why that is
#    required, not a tuning choice)
docker compose up -d
docker compose ps

# 3. Moon needs no schema migration and no role bootstrap. That is the
#    whole of what used to be step 3.

# 4. Point the binary at Moon and run
export LUNARIS_STORE_URL="moon://127.0.0.1:6380"
cargo run --release
```

Expected output:

```
quickstart: opening lunaris handle at moon://127.0.0.1:6380
quickstart: ingested episode at lsn=Lsn(1) under scope `quickstart`
quickstart: recalled 1 hit(s) for "hello"
quickstart:   top hit score=0.83 text="# Hello from Lunaris …"
quickstart: DSL form returned 1 hit(s)
```

## Tear-down

```bash
docker compose down -v   # -v wipes the Moon data volume
```

## Graph extraction (optional)

Extraction and verification are remote-only since the llama.cpp cutover:

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
- `connection refused` → Moon is still starting. Re-check
  `docker compose ps`, or `docker compose logs moon`.
- `TXN does not support cross-shard writes` on ingest → Moon is running
  sharded. It MUST run with `--shards 1`; the published image's own CMD
  uses `--shards 0` (auto-detect), which is why the compose file overrides
  the whole command.
- `` `postgres://` was removed in 0.7.0 `` → `LUNARIS_STORE_URL` still
  points at the old backend. See
  [`docs/migration/0.6-to-0.7.md`](../../docs/migration/0.6-to-0.7.md).

## What's next

- `examples/quickstart-py/` — same flow via `pip install lunaris`.
- `examples/quickstart-ts/` — same flow via `npm i lunaris`.
- `docs.lunaris.dev` — full concepts (bi-temporal, MVCC, atomic write,
  ACT-R consolidator).
