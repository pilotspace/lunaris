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

## Recall walkthrough (Phase 23 follow-up)

Same shape as the Python and Rust quickstarts — the typed `Scope` +
`EpisodeBuilder` TypeScript surface lands in v0.3. The DSL side is
already wired:

```typescript
const hits = await handle.recall().vector("hello", { topK: 3 }).execute();
```

A full recall walkthrough lands alongside the Rust example once the
v0.2.x DSL stabilises for OSS adoption.

## Troubleshooting

- `Error: Cannot find module 'lunaris'` → run `npm install` (or
  `npm install ../../crates/lunaris-ts` for local dev).
- `LunarisError: embedding-gemma weights missing` → the default
  features built a candle-backed embedder and the weights aren't
  cached. Rebuild the `.node` without the `candle` feature or
  pre-download the weights.
- `postgres connection refused` → wait for the docker-compose
  healthcheck (`docker compose ps -f ../quickstart-rs/docker-compose.yml`).
- `relation "episodes" does not exist` → re-run step 3.
