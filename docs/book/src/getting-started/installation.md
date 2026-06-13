# Installation

**Add Lunaris to a Rust, Python, or TypeScript project, stand up the
storage backend it needs, and (optionally) run the HTTP server.** For
the exhaustive list of feature flags and `LUNARIS_*` environment
variables, see the [Configuration Reference](../reference/configuration.md).

## Prerequisites

Lunaris needs a storage backend and an embedder. The connection-string
scheme picks the backend:

| URL | Backend | Needs |
|---|---|---|
| `memory://` | embedded (SQLite, in-process) | **nothing** — start here |
| `sqlite:///path/to/lunaris.db` | embedded (SQLite, file-backed) | nothing |
| `postgres://…` / `postgresql://…` | Postgres + pgvector + pgmq | a database (the shipped image, or any Postgres with the extensions) |
| `moon://host:port` | Moon | a running Moon instance |

### Storage: `memory://` — zero dependencies (start here)

```rust
let lunaris = lunaris::Lunaris::open("memory://").await?;
```

No Docker, no Postgres, no Moon. Backed by an in-process SQLite database
(`sqlite:///path` for a file you keep). Ideal for the quickstart, local
development, and tests.

> **Embedded-backend status.** The embedded backend currently implements
> the bi-temporal KV core (`ingest`, recall-by-key, time-travel reads) with
> the same atomic-write guarantee as Postgres. Vector / graph / BM25 search
> and the consolidation queue are *not yet wired* on this backend — use
> Postgres or Moon for those. See
> [Choosing a Backend](../operations/backends.md).

### Storage: Postgres (the portable production default)

The Postgres backend requires **`pgvector`** and **`pgmq`** (queue), and
optionally **Apache AGE** for the graph operators. A stock managed Postgres
that has pgvector + pgmq works out of the box; the repo also ships a
ready-built image that bundles all three:

- **[`scripts/pg-lunaris/`](https://github.com/pilotspace/lunaris/tree/main/scripts/pg-lunaris)**
  — `Dockerfile` building `postgres:16` + `pgvector` + AGE + pgmq.
- **[`examples/quickstart-rs/docker-compose.yml`](https://github.com/pilotspace/lunaris/blob/main/examples/quickstart-rs/docker-compose.yml)**
  — wraps that image with a healthcheck and a data volume on
  `localhost:5432`. The Python and TS quickstarts reuse it via
  `docker compose -f ../quickstart-rs/docker-compose.yml up -d`.

```bash
cd examples/quickstart-rs
docker compose up -d
docker compose ps        # wait until lunaris-quickstart-pg is "healthy"
```

**Migrations are applied for you** — there is no `sqlx migrate run` step:

- The simple path: `Lunaris::open("postgres://…")` runs the embedded
  migration set automatically when the connecting role can run DDL.
- The production path (RLS requires a `NOSUPERUSER NOBYPASSRLS` app role
  that *can't* run DDL): one command provisions everything —

  ```bash
  lunaris-server bootstrap-db \
    --admin-url postgres://admin:pw@localhost:5432/lunaris \
    --app-role  lunaris_app \
    --app-password '…'
  ```

  This runs the migrations as the admin role, creates/repairs
  `lunaris_app` with the right grants, and reports any RLS hardening gap.
  Then run the app against `postgres://lunaris_app:…@host/lunaris`.
- Or let the server self-migrate on start: set
  `LUNARIS_ADMIN_URL=postgres://admin:…@host/lunaris` alongside the app
  `--storage` URL — migrations run over the admin connection, the runtime
  binds the app role.
- To run only the migrations (e.g. in CI): `lunaris-server migrate
  --storage postgres://admin:…@host/lunaris`.

See [Choosing a Backend](../operations/backends.md) for the trade-offs and
the embedding-dimension story (the Moon adapter sizes its vector index to
the embedder — default 768-d, set wider via `connect_with_dim`; pgvector
handles up to ~1536-d).

### Storage: Moon (the high-performance substrate)

If you run [Moon](https://github.com/pilotspace/lunaris), point Lunaris
at `moon://host:port`. Moon provides native `FT.SEARCH` (vector + BM25),
`GRAPH.QUERY`, a message queue, and **native RRF fusion** — the
`fuse_rrf` operator collapses a (Vector + Keyword) pair on the same
index into one round trip. No Moon repo? Stick with Postgres; every
`Lunaris` call works identically against either backend.

### Embedder: in-process native, no external service required

The default embedder is
**granite-embedding-311m-multilingual-r2** (768-d), runs **in-process** via
candle, and auto-downloads to `~/.cache/lunaris/models/` on first use — **no
Ollama, no ONNX Runtime, no external service required.** An air-gapped Ollama
HTTP embedder remains available as an operator escape hatch behind
`--features embed-remote`.

Override the model directory with `LUNARIS_EMBEDDER_DIR=<path>`. For
RSS-constrained hosts, activate the Q4_K_M GGUF variant via
`--features embedder-gguf` and set `LUNARIS_EMBEDDER_GGUF=<path/to/granite-r2.Q4_K_M.gguf>`.
Operators running an air-gapped Ollama for embedding (not the supported path)
can set `LUNARIS_EMBEDDER_OLLAMA_URL=<endpoint>` alongside `--features embed-remote`.

See the
[Configuration Reference](../reference/configuration.md#1-cargo-feature-flags)
for the full feature matrix.

## Rust

The umbrella crate is published as **`lunaris-memory`** (the bare `lunaris`
name on crates.io is owned by an unrelated project); the library itself is
still named `lunaris`, so you import it as `use lunaris::…`. Rename it in
`Cargo.toml`:

```toml
[dependencies]
lunaris = { package = "lunaris-memory", version = "0.4" }
tokio   = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```bash
cargo add lunaris-memory --rename lunaris
cargo add tokio --features macros,rt-multi-thread
```

Feature flags on the `lunaris` umbrella crate (full table in the
[Configuration Reference](../reference/configuration.md#1-cargo-feature-flags)):

> **Note.** The native granite-r2 embedder and bge-reranker-v2-m3 reranker
> are **always compiled** — no feature gate is needed. The flags below only
> gate optional backends for the extractor, verifier, and escape-hatch paths.

| Feature | Default | Effect |
|---|:--:|---|
| `embedder-gguf` | | Q4_K_M GGUF quantized native embedder; activated by `LUNARIS_EMBEDDER_GGUF` |
| `reranker-gguf` | | Q5_K_M-imatrix GGUF quantized native reranker; activated by `LUNARIS_RERANKER_GGUF` |
| `embed-remote` | | Ollama HTTP embedder **escape hatch** (operator-only, not the supported path) |
| `candle` | | In-process candle **extractor** (Gemma-3 4B) + **verifier** (Gemma-3 27B) LLM backends |
| `ollama` | | Ollama HTTP **extractor** + **verifier** backends (NOT the embedder) |
| `cloud-api` | | Cloud-API extractor / verifier backends (pulls `reqwest`) |
| `verify-small` | | In-process candle Gemma-3-270M verifier — the laptop-floor build (~600 MB disk / ~1 GB RAM, RFC 0006) |
| `verify-large` | | In-process candle Gemma-3-27B verifier (explicit alias per RFC 0006) |

`default = []` — the native embedder + reranker are unconditional; no
feature is required for the core recall path. The extractor and verifier
pipelines are opt-in at build time.

Smoke test — no services needed:

```rust
#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = lunaris::Lunaris::open("memory://").await?;
    println!("{lunaris:?}");
    Ok(())
}
```

Swap `"memory://"` for `"postgres://…"` or `"moon://…"` when you're ready
to point at a real backend — every `Lunaris` call works identically.

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

## TypeScript — `npm i @pilotspace/lunaris`

```bash
npm i @pilotspace/lunaris
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

`lunaris-server` also has two operational subcommands (Postgres only):

- `lunaris-server migrate --storage <admin_url>` — apply the embedded
  migration set and exit.
- `lunaris-server bootstrap-db --admin-url <admin_url> [--app-role lunaris_app] --app-password <pw>`
  — migrate, then create/repair the `NOSUPERUSER NOBYPASSRLS` app role
  with the right grants, then report any RLS hardening gap.

Set `LUNARIS_ADMIN_URL` alongside `--storage` (the app-role URL) to have
the server migrate over the admin connection on start.

## Next

- [10-Minute Quickstart](./quickstart.md) — ingest and recall your
  first episode, Rust / Python / TypeScript side by side.
- [Core Concepts](./concepts.md) — episodes, scope, bi-temporal MVCC,
  the atomic write.
- [Configuration Reference](../reference/configuration.md) — every
  feature flag and `LUNARIS_*` variable.
