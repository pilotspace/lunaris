# Lunaris Quickstart — TypeScript

Mirrors the [Rust](../quickstart-rs/README.md) and [Python](../quickstart-py/README.md)
quickstarts against the same Postgres backend.

## Prerequisites

- `docker` + `docker compose` v2.20+
- Node 20+ (`napi-rs` prebuilds target abi-modules-x64-musl/glibc on Node 20)
- `ollama` 0.3+ running locally

## Five steps

```bash
# 1. From the repo root
cd examples/quickstart-ts

# 2. Reuse the Rust quickstart's Postgres image
docker compose -f ../quickstart-rs/docker-compose.yml up -d

# 3. Apply migrations
sqlx migrate run --source ../../crates/lunaris-storage-postgres/migrations \
                 --database-url postgres://lunaris:lunaris@localhost:5432/lunaris

# 4. Install lunaris + the tsx runner
npm install                  # picks up lunaris from package.json
ollama serve & ollama pull nomic-embed-text

# 5. Point at Postgres and run
export LUNARIS_PG_URL="postgres://lunaris:lunaris@localhost:5432/lunaris"
npm start                    # = npx tsx quickstart.mts
```

Expected output:

```
quickstart: opening lunaris handle at postgres://...
quickstart: ingested episode at lsn=...:... under scope `quickstart`
quickstart: ingest path verified; see README for recall walkthrough
```

## Local-dev variant (no npm release yet)

If you're developing against this repo before the npm release:

```bash
cd ../../crates/lunaris-ts
npm run build                # napi build → .node prebuild
cd ../../examples/quickstart-ts
npm install ../../crates/lunaris-ts
npx tsx quickstart.mts
```

`napi build` compiles the lunaris-ts `.node` binding in-place. The
local install resolves to that build, so the script imports the
just-built binding.

## Recall — current binding limitations (v0.2.x)

The Rust quickstart ([../quickstart-rs/](../quickstart-rs/)) does a
real scoped recall. The TypeScript binding is **not there yet** — two
gaps, both v0.3 deliverables:

1. **No scope-aware recall.** `handle.recall().…​.execute()` exists, but
   it always queries the default `_dev_` partition — there's no scope
   parameter on the TS builder, so it can't see the `quickstart` scope
   this script ingests into. (The Rust path threads a real `Scope`
   through ingest *and* recall.)
2. **No query-text parameter.** The TS DSL builder (`new Vector(index,
   k)`, `.top(n)`, `.fuseRrf(k)`, `.asOf(ms)`, `.filter(...)`) collapses
   to a plan whose `query` field is always the empty string — the FFI
   bridge `recallSimpleExecute` accepts the `Vector("chunks", k)`
   default shape only (see `crates/lunaris-ts/src/dsl.rs`). So
   `handle.recall().execute()` returns hits for an *empty* query, not a
   useful one. A real query string lands when the FFI surface is
   widened in v0.3.

The typed `Scope` + `EpisodeBuilder` TypeScript surface also lands in
v0.3; today the wire shape is a plain object (mirrors
`lunaris_core::primitives::Episode`).

Until then, this quickstart stops at the ingest contract. Follow the
Rust quickstart for the end-to-end recall walkthrough.

## Troubleshooting

- `Error: Cannot find module 'lunaris'` → run `npm install` (or
  `npm install ../../crates/lunaris-ts` for local dev).
- Vector recall returns empty rows / a `WARN` about the embedder →
  the granite-r2 Q4_K_M GGUF isn't staged. Download it to
  `~/.lunaris/models/` (SHA-256s:
  `cargo run -p lunaris-bench --bin stage-models -- --help`) or pass
  `EmbedderConfig.llamacpp({ ggufPath })` explicitly.
- `postgres connection refused` → wait for the docker-compose
  healthcheck (`docker compose ps -f ../quickstart-rs/docker-compose.yml`).
- `relation "episodes" does not exist` → re-run step 3.
