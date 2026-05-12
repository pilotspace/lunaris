# Installation

**Add Lunaris to a Rust, Python, or TypeScript project, stand up the
storage backend it needs, and (optionally) run the HTTP server.** For
the exhaustive list of feature flags and `LUNARIS_*` environment
variables, see the [Configuration Reference](../reference/configuration.md).

## Prerequisites

Lunaris needs a storage backend and an embedder. The defaults are tuned
for *"Postgres + a local embedder, graph and verifier off"* — the safe
production floor.

### Storage: Postgres (the portable default)

Lunaris's Postgres backend uses `pgvector`, Apache **AGE** (graph), and
**pgmq** (queue) — three extensions you won't find in a stock `postgres`
image. The repo ships a ready-built image:

- **[`scripts/pg-lunaris/`](https://github.com/lunaris-dev/lunaris/tree/main/scripts/pg-lunaris)**
  — `Dockerfile` that builds `postgres:16` + `pgvector` + AGE + pgmq,
  plus an `init-extensions.sql` that `CREATE EXTENSION`s them on first
  boot.
- **[`examples/quickstart-rs/docker-compose.yml`](https://github.com/lunaris-dev/lunaris/blob/main/examples/quickstart-rs/docker-compose.yml)**
  — wraps that image with a healthcheck and a data volume on
  `localhost:5432`. The Python and TS quickstarts reuse it via
  `docker compose -f ../quickstart-rs/docker-compose.yml up -d`.

```bash
# From the repo root:
cd examples/quickstart-rs
docker compose up -d
docker compose ps        # wait until lunaris-quickstart-pg is "healthy"
```

Migrations are applied automatically the first time `Lunaris::open`
connects with a DDL-capable role (sqlx-managed, from
`crates/lunaris-storage-postgres/migrations/`). For a non-privileged app
role that should *not* run DDL — required so Postgres RLS actually
applies — use the `NOSUPERUSER NOBYPASSRLS` recipe and apply migrations
out of band:

```bash
sqlx migrate run --source crates/lunaris-storage-postgres/migrations \
                 --database-url postgres://lunaris:lunaris@localhost:5432/lunaris
```

The connection-string scheme picks the backend: `postgres://…` /
`postgresql://…` → Postgres; `moon://host:port` → Moon. See
[Choosing a Backend](../operations/backends.md) for the trade-offs and
the dimension caps (Postgres ≤ 1536-d vectors, Moon ≤ 768-d).

### Storage: Moon (the high-performance substrate)

If you run [Moon](https://github.com/lunaris-dev/lunaris), point Lunaris
at `moon://host:port`. Moon provides native `FT.SEARCH` (vector + BM25),
`GRAPH.QUERY`, a message queue, and **native RRF fusion** — the
`fuse_rrf` operator collapses a (Vector + Keyword) pair on the same
index into one round trip. No Moon repo? Stick with Postgres; every
`Lunaris` call works identically against either backend.

### Embedder: default needs no Ollama

The default embedder backend is **`fastembed`** (ONNX EmbeddingGemma-300M)
— it auto-downloads weights to `~/.cache/lunaris/models/fastembed/` on
first use and runs in-process. **No Ollama required.**

Two optional alternatives:

- **Ollama** (`LUNARIS_EMBEDDER_BACKEND=ollama`, requires the `ollama`
  Cargo feature) — points at `http://localhost:11434`. Some quickstart
  runbooks use `ollama pull nomic-embed-text` for a tiny model; this is
  optional, not the default.
- **candle** (`LUNARIS_EMBEDDER_BACKEND=candle`, requires the `candle`
  feature) — fully in-process Gemma-300M, air-gapped; loads weights from
  `~/.cache/lunaris/models/embedding-gemma-300m/` (the `LUNARIS_EMBED_GEMMA_PATH`
  env var points it at a different directory).

Backend *availability* is fixed at build time by Cargo features;
`LUNARIS_*_BACKEND` only chooses among what was compiled in. Asking for
`candle` in a `fastembed`-only build is a startup error. See the
[Configuration Reference](../reference/configuration.md#1-cargo-feature-flags)
for the full feature matrix.

## Rust

The umbrella crate is published as **`lunaris-memory`** (the bare `lunaris`
name on crates.io is owned by an unrelated project); the library itself is
still named `lunaris`, so you import it as `use lunaris::…`. Rename it in
`Cargo.toml`:

```toml
[dependencies]
lunaris = { package = "lunaris-memory", version = "0.2" }
tokio   = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```bash
cargo add lunaris-memory --rename lunaris
cargo add tokio --features macros,rt-multi-thread
```

Feature flags on the `lunaris` umbrella crate (full table in the
[Configuration Reference](../reference/configuration.md#1-cargo-feature-flags)):

| Feature | Default | Effect |
|---|:--:|---|
| `fastembed` | ✅ | ONNX `fastembed` embedder + reranker backends (auto-downloads weights) |
| `candle` | ✅ | In-process `candle` embedder, reranker, extractor, verifier backends |
| `ollama` | | Ollama HTTP extractor / verifier / embedder backends |
| `cloud-api` | | Cloud-API extractor / verifier backends (pulls `reqwest`) |
| `verify-small` | | In-process candle Gemma-3-270M verifier — the laptop-floor build (~600 MB disk / ~1 GB RAM, RFC 0006) |

`default = ["fastembed", "candle"]`. A `cargo build --no-default-features`
build links neither stack — useful for the HTTP-only server image, but
then you must construct the handle via `Lunaris::with_parts(...)` instead
of `Lunaris::open(url)`.

Smoke test:

```rust
#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = lunaris::Lunaris::open(
        "postgres://lunaris:lunaris@localhost:5432/lunaris",
    ).await?;
    println!("{lunaris:?}");
    Ok(())
}
```

## Python — `pip install lunaris`

```bash
pip install lunaris          # or: uv add lunaris
```

PyO3 0.26 baseline; Python 3.11+. The wheel bundles the compiled
extension — no Rust toolchain needed at install time. The Python surface
mirrors the Rust handle: `await lunaris.open(url)` →
`await handle.ingest(episode)` → `await handle.recall()`. See the
[Python SDK chapter](../sdk/python.md) for the full surface.

> Developing against this repo before the PyPI release? Build the
> binding in place with `maturin develop --release` from
> `crates/lunaris-py/`, then `import lunaris` resolves to the local
> build. `lunaris-py` is a `cdylib` and is excluded from
> `cargo test --workspace`.

## TypeScript — `npm i lunaris`

```bash
npm i lunaris
```

napi-rs 3.x, Node 20+ ABI. Prebuilt `.node` binaries ship in the
package. The surface mirrors the Rust handle: `await lunaris.open(url)`
→ `await handle.ingest(episode)` → `await handle.recall()`. See the
[TypeScript SDK chapter](../sdk/typescript.md).

> Developing against this repo before the npm release? Run
> `npm run build` (= `napi build`) from `crates/lunaris-ts/`, then
> `npm install ../../crates/lunaris-ts` from your project. `lunaris-ts`
> is a `cdylib` and is excluded from `cargo test --workspace`.

## Running the HTTP server

Non-Rust runtimes talk to Lunaris through `lunaris-server` — an axum
service implementing MemoryProtocol 0.1 (`/v1/{ingest,recall,forget,snapshot}`,
HTTP + SSE). It needs a storage URL and a bearer-token map:

```bash
# One-time: a bearer-token map. Every /v1/* request needs a token from it.
cat > /tmp/lunaris-tokens.json <<'EOF'
{
  "dev-token-xxx": { "tenant": "acme", "scopes": ["acme.agent-1", "acme.agent-2"] }
}
EOF

cargo run -p lunaris-server -- \
  --storage postgres://lunaris:lunaris@localhost:5432/lunaris \
  --bind 0.0.0.0:8080 \
  --tokens-file /tmp/lunaris-tokens.json
```

Every CLI flag has a matching `LUNARIS_*` env var (the CLI flag wins).
The key knobs:

| Flag | Env | Default |
|---|---|---|
| `--bind` | `LUNARIS_BIND` | `0.0.0.0:8080` |
| `--storage` | `LUNARIS_STORAGE` | *(required)* |
| `--tokens-file` | `LUNARIS_TOKENS_FILE` | *(required)* |
| `--rate-per-second` | `LUNARIS_RATE_PER_SECOND` | `60` |
| `--rate-burst` | `LUNARIS_RATE_BURST` | `120` |
| `--shutdown-grace-secs` | `LUNARIS_SHUTDOWN_GRACE_SECS` | `30` |

The `tenant` claim from the bearer token is the **only** source of truth
for the partition scope — route handlers ignore any `scope` / `tenant`
field on the request body. Probe surfaces `/healthz` and `/metrics` are
unauthenticated. Full route list, DTOs, and the SSE contract:
[Running the HTTP Server](../operations/server.md) and the
[MemoryProtocol 0.1 spec](../protocol/memoryprotocol-0.1.md).

## Next

- [10-Minute Quickstart](./quickstart.md) — ingest and recall your
  first episode, Rust / Python / TypeScript side by side.
- [Core Concepts](./concepts.md) — episodes, scope, bi-temporal MVCC,
  the atomic write.
- [Configuration Reference](../reference/configuration.md) — every
  feature flag and `LUNARIS_*` variable.

> **Note on links.** `github.com/lunaris-dev/lunaris` and `lunaris.dev`
> are placeholders pending the final OSS home.
