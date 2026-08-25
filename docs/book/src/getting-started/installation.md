# Installation

**Add Lunaris to a Rust, Python, or TypeScript project, stand up the
storage backend it needs, and (optionally) run the HTTP server.** For
the exhaustive list of feature flags and `LUNARIS_*` environment
variables, see the [Configuration Reference](../reference/configuration.md).

## Prerequisites

Lunaris needs a running Moon and an embedder. As of **0.7.0 there is one
storage scheme**:

| URL | Backend | Needs |
|---|---|---|
| `moon://host:port` | Moon | a running Moon started with `--shards 1` |

Every retired spelling — `memory://`, `sqlite:///path`, `postgres://…` —
is rejected by `Lunaris::open` with an `UnsupportedScheme` error carrying
the migration link. The Postgres and SQLite backends were deleted in
0.7.0; if you have data in one, migrate it **before** you bump your
`lunaris` pin — see
[0.6 → 0.7](https://github.com/pilotspace/lunaris/blob/main/docs/migration/0.6-to-0.7.md).

### Storage: Moon

```bash
docker run -d --name lunaris-moon -p 6380:6379 \
  ghcr.io/pilotspace/moon:0.8.5 \
  --shards 1 --protected-mode no --appendonly yes
```

```rust,no_run
# use lunaris::Lunaris;
# async fn demo() -> Result<(), lunaris::LunarisError> {
let lunaris = lunaris::Lunaris::open("moon://127.0.0.1:6380").await?;
# Ok(())
# }
```

`--shards 1` is **not optional**: a Lunaris ingest is one MULTI/EXEC
transaction, and a sharded Moon rejects cross-shard writes. The image
defaults to `--shards 0` (auto), so the flag has to be passed explicitly.
`--appendonly yes` is what makes the store survive a restart.

Moon provides native `FT.SEARCH` (vector + BM25), `GRAPH.QUERY`, a message
queue, and **native RRF fusion** — the `fuse_rrf` operator collapses a
(Vector + Keyword) pair on the same index into one round trip.

Full production setup — persistence, memory limits, backups, health
probes — is in
[Running an external Moon](https://github.com/pilotspace/lunaris/blob/main/docs/operations/external-moon.md). See
[Choosing a Backend](../operations/backends.md) for the
embedding-dimension story (the Moon adapter sizes its vector index to the
embedder — default 768-d, set wider via `connect_with_dim`).

### Embedder: in-process llama.cpp, no external service required

> **v0.6 llama.cpp-only cutover.** The candle inference stack is deleted.
> The only local embed/rerank runtime is
> `lunaris-llamacpp` — in-process llama.cpp FFI, static-linked, no external
> server. See `docs/decisions/2026-07-10-llamacpp-only-cutover.md` (the
> cutover ADR) and `docs/migration/0.5-to-0.6-llamacpp-only.md` (the
> migration guide).

The default embedder is **granite-embedding-311m-multilingual-r2** (768-d),
loaded from a **Q4_K_M GGUF** (~240 MiB) via in-process llama.cpp — **no
Ollama, no external service required.** The default reranker is
**bge-reranker-v2-m3** (Q5_K_M GGUF, ~446 MiB), lazy-loaded on first recall.
Both GGUFs are expected at `~/.lunaris/models/`; the MCP server stages them
lazily on first recall, other deployments download them out-of-band. An
air-gapped Ollama HTTP embedder remains available as an operator escape hatch
behind `--features embed-remote` (resolves **after** the llama.cpp step).

Override the artifact paths with `LUNARIS_EMBEDDER_GGUF=<path>` /
`LUNARIS_RERANKER_GGUF=<path>`. There is no auto-download in the umbrella
crate — a missing GGUF logs a `WARN` and falls back to `NoopEmbedder` /
`NoopReranker`. Operators running an air-gapped Ollama for embedding (not the
supported path) can set `LUNARIS_EMBEDDER_OLLAMA_URL=<endpoint>` alongside
`--features embed-remote`.

Building the `llamacpp` feature (default) needs **cmake + a C++ toolchain**.
For a pure-Rust, no-inference build (Tier-0, small devices):
`default-features = false` → `NoopEmbedder`/`NoopReranker`.

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
lunaris = { package = "lunaris-memory", version = "0.6" }
tokio   = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```bash
cargo add lunaris-memory --rename lunaris
cargo add tokio --features macros,rt-multi-thread
```

Feature flags on the `lunaris` umbrella crate (full table in the
[Configuration Reference](../reference/configuration.md#1-cargo-feature-flags)):

> **Note.** `llamacpp` is the only local embed/rerank runtime and is
> **on by default** — building it needs cmake + a C++ toolchain. The flags
> below also gate the extractor/verifier remote-provider backends and the
> escape-hatch paths.

| Feature | Default | Effect |
|---|:--:|---|
| `llamacpp` | ✅ | In-process llama.cpp embedder (granite-r2 Q4_K_M GGUF) + reranker (bge-reranker-v2-m3 Q5_K_M GGUF) |
| `metal` / `cuda` / `vulkan` | | GPU offload — forwards to `llama-cpp-2`'s backends |
| `embed-remote` | | Ollama HTTP embedder **escape hatch** (operator-only, not the supported path); resolves after the `llamacpp` step |
| `ollama` | | `OllamaExtractor` / Ollama HTTP **verifier** backend selector (NOT the embedder) |
| `cloud-api` | | Cloud-API extractor / verifier backends (pulls `reqwest`) |

`default = ["llamacpp"]`. Extractor and verifier are **remote-only** —
`LUNARIS_EXTRACT_PROVIDER` / `LUNARIS_VERIFY_PROVIDER`
(`anthropic`\|`openai`\|`gemini`\|`minimax`\|`openai-compat`, the last
covering Ollama / llama-server / vLLM / LM Studio via
`LUNARIS_OPENAI_COMPAT_BASE_URL`) or a caller-supplied `with_extractor` /
`with_verifier` impl; unset resolves to `NoopExtractor`/`NoopVerifier`. For a
pure-Rust, no-C++-toolchain build (Tier-0): `default-features = false`.

Smoke test — needs the Moon from
[Prerequisites](#storage-moon) running:

```rust,no_run
#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = lunaris::Lunaris::open("moon://127.0.0.1:6380").await?;
    println!("{lunaris:?}");
    Ok(())
}
```

There is no zero-dependency scheme to start from since 0.7.0 — `open`
either reaches a Moon or returns an error naming what to start.

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
  --storage moon://127.0.0.1:6380 \
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

> **Removed in 0.7.0.** `lunaris-server migrate` and `lunaris-server
> bootstrap-db` were Postgres-only (embedded migration set, RLS app-role
> provisioning) and went with the backend, along with `LUNARIS_ADMIN_URL`.
> Moon needs no schema migration and no role bootstrap — start it, point
> `--storage` at it. Indexes are created on first connect.

## Next

- [10-Minute Quickstart](./quickstart.md) — ingest and recall your
  first episode, Rust / Python / TypeScript side by side.
- [Core Concepts](./concepts.md) — episodes, scope, bi-temporal MVCC,
  the atomic write.
- [Configuration Reference](../reference/configuration.md) — every
  feature flag and `LUNARIS_*` variable.
