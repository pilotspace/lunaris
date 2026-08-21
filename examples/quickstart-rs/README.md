# Lunaris Quickstart — Rust

Goal: from a fresh checkout, run your first Lunaris ingest + recall against a
local Moon.

**0.7.0 is Moon-only.** The Postgres and SQLite backends, and `lunaris-migrate`,
were all deleted. If you have 0.6.x data to bring across, run `lunaris-migrate`
from the **v0.6.2 release binary before upgrading** — see
[`docs/migration/0.6-to-0.7.md`](../../docs/migration/0.6-to-0.7.md).

## Read this before you start: you need a Moon binary, and there isn't a download yet

Lunaris 0.7.0 refuses any Moon below `0.8.5` at connect
(`crates/lunaris-storage-moon/src/version.rs`). **Moon v0.8.5 was published with
zero release assets**: every platform tarball 404s, and an anonymous
`docker pull ghcr.io/pilotspace/moon:0.8.5` gets a `401`. The v0.8.4 binaries
that do exist are rejected by the handshake.

So the `docker compose up -d` step below **cannot work today**. It is kept
because the compose file is correct and will start working the moment the
assets are re-cut (tracked as W0.1 in `docs/planning/2026-08-21-ship-plan.md`).
Until then use one of the two paths that do work:

### Path A — you just want to see Lunaris work (no server at all)

Skip this example. `lunaris try` runs ingest, indexing and recall inside one
process against an in-process Moon:

```bash
cargo build --release -p lunaris-cli --features embedded-moon
./target/release/lunaris try
```

See [`docs/quickstart-try.md`](../../docs/quickstart-try.md). (There is no
prebuilt `lunaris` binary yet either — that is ship-plan task W0.9 — so the
`cargo build` line is not optional.)

### Path B — you want a real Moon this example can talk to

Build one from the vendored submodule. This is the only Moon 0.8.5 in
existence right now:

```bash
# from the repo root
git submodule update --init vendor/moon
cargo build --release --manifest-path vendor/moon/Cargo.toml --bin moon

vendor/moon/target/release/moon --port 6380 --shards 1 --protected-mode no
```

`--shards 1` is mandatory, not a tuning choice — see *Troubleshooting*.

## Prerequisites

- `cargo` 1.94+ (Rust edition 2024 toolchain)
- a C++ toolchain + `cmake` — the default build embeds llama.cpp
- the granite-r2 Q4_K_M GGUF staged at `~/.lunaris/models/` for that embedder
  (override with `LUNARIS_EMBEDDER_GGUF`)
- a single-shard Moon on `127.0.0.1:6380` (Path B above, or `docker compose up -d`
  once W0.1 lands)

If you have no C++ toolchain, build Tier-0 instead
(`cargo run --release --no-default-features --features ollama`) and point
`LUNARIS_EMBEDDER_OLLAMA_URL` at a local Ollama. Vector ranking is meaningless
without *some* embedder — a Tier-0 build with nothing configured falls through
to the zero-vector `NoopEmbedder`.

> If port 6380 is already taken on your machine, pick another one and change
> both the port and `LUNARIS_STORE_URL` together.

## Run it

```bash
cd examples/quickstart-rs
export LUNARIS_STORE_URL="moon://127.0.0.1:6380"
cargo run --release
```

Moon needs no schema migration and no role bootstrap — there is no step
between starting the server and running the binary.

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

(Or just stop the Path B process; its data lives wherever you started it.)

## Graph extraction (optional)

Extraction and verification are remote-only since the llama.cpp cutover:

```bash
export LUNARIS_GRAPH_ENABLED=1
export LUNARIS_EXTRACT_PROVIDER=openai-compat
export LUNARIS_OPENAI_COMPAT_BASE_URL=http://127.0.0.1:11434/v1
export OPENAI_COMPAT_EXTRACT_MODEL=gemma3:4b
cargo run --release
```

See [`docs/migration/0.5-to-0.6-llamacpp-only.md`](../../docs/migration/0.5-to-0.6-llamacpp-only.md)
for the full provider matrix (anthropic | openai | gemini | minimax | openai-compat).

## Recall walkthrough

The binary recalls the episode it just ingested, scoped to the bound `Scope`.
Two equivalent forms:

```rust
// 1. The one-liner — the retrieval root is Vector::new("chunks", 30).
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

`recall` returns `Vec<lunaris::Hit>`; each `Hit` carries `id`, `score`, `text`,
`source`, `heading_path`, `valid_from` / `valid_to`, plus the `degraded` /
`rerank_applied` / `source_op` provenance flags. See
`crates/lunaris-retrieve/src/types.rs`.

**This DSL form is Rust-only.** The Python and TypeScript bindings expose a
`scoped.dsl()` method, but it returns the codegen-frozen native
`RetrievalBuilder`: its combinators raise `NotImplementedError` and it has no
`execute()`. Both SDK quickstarts therefore stop at `scoped.recall(text)`.

For the full DSL surface (BM25 keyword, graph traversal, RRF fusion,
cross-encoder rerank, `as_of` time-travel) see the retrieval DSL guide at
`docs.lunaris.dev`.

## Troubleshooting

- `unsupported Moon version` / a handshake rejection → your Moon is older than
  0.8.5. See the section at the top; 0.8.4 binaries will not do.
- Vector recall returns empty rows / a `WARN` about the embedder → the
  granite-r2 Q4_K_M GGUF isn't staged at `~/.lunaris/models/`. Download it
  out-of-band (SHA-256s: `cargo run -p lunaris-bench --bin stage-models -- --help`)
  or set `LUNARIS_EMBEDDER_GGUF` to an existing copy.
- `connection refused` → Moon is still starting. Re-check `docker compose ps`,
  or `docker compose logs moon`.
- `TXN does not support cross-shard writes` on ingest → Moon is running
  sharded. It MUST run with `--shards 1`; the published image's own CMD uses
  `--shards 0` (auto-detect), which is why the compose file overrides the whole
  command.
- `` `postgres://` was removed in 0.7.0 `` → `LUNARIS_STORE_URL` still points at
  the old backend. See
  [`docs/migration/0.6-to-0.7.md`](../../docs/migration/0.6-to-0.7.md).

## What's next

- [`examples/quickstart-py/`](../quickstart-py/) — same flow via `pip install lunaris`.
- [`examples/quickstart-ts/`](../quickstart-ts/) — same flow via `npm i @pilotspace/lunaris`.
- [`examples/multi-agent-rs/`](../multi-agent-rs/) — two agents, hard scope isolation, durability across a process boundary.
- `docs.lunaris.dev` — full concepts (bi-temporal, MVCC, atomic write, ACT-R consolidator).
