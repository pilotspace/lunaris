# TypeScript SDK

**`npm install @pilotspace/lunaris` gives you the same memory engine as the Rust crate,
behind a napi-rs 3.x binding generated from the same annotated surface** — so
`open / ingest / recall / forget / snapshot` and the composable retrieve DSL
behave identically across all three SDKs. `cargo run -p lunaris-codegen --
--check` gates every PR, so the surfaces never drift.

> Adapted from `docs/bindings.md` (TypeScript half).

## Install

```bash
npm install @pilotspace/lunaris
```

Prebuilt `.node` binaries ship for 5 targets (BIND-TS-05): `linux-x64`,
`linux-arm64`, `darwin-x64`, `darwin-arm64`, `win32-x64`.

### Node 20 ABI

The NAPI ABI is pinned to **version 8** (Node 20 LTS) via the `napi8` feature
on the Rust `napi` dep. The `abi_pin.spec.mts` test asserts
`process.versions.napi >= 8` at startup, so an older runtime (Node 18, which
ships NAPI 7) fails with a readable reason instead of a cryptic
`undefined symbol: napi_get_value_*` at `dlopen` time. **Use Node 20 LTS or
later.**

No matching binary? Source install (needs Rust 1.94+ and `@napi-rs/cli`):

```bash
npm install @pilotspace/lunaris --build-from-source
```

## Quickstart

```typescript
import { open, RetrievalBuilder } from "@pilotspace/lunaris";

async function main() {
  const handle = await open("moon://127.0.0.1:6380");

  const lsn = await handle.ingest({
    id: "01JABCDEFGHJKMNPQRSTVWXYZ0", // 26-char Crockford-base32 ULID
    source: "ts-quickstart",
    content: "Lunaris bi-temporal memory — hello from TypeScript.",
    metadata: {},
    t_ref: null,
    bt: {
      valid: [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
      sys:   [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
    },
  });
  console.log("ingested at LSN", lsn);

  const hits = await new RetrievalBuilder().bind(handle).top(5).execute();
  for (const h of hits) console.log(h);
}

main();
```

> `RetrievalBuilder` / `Vector` / `Keyword` / `Graph` imported from `lunaris`
> are the pure-JS plan builders the package's ESM entry layers on top of the
> raw napi-rs classes — call `.bind(handle)` before `.execute()`.
> `handle.recall()` returns the *raw* napi `RetrievalBuilder`, whose builder
> methods are not-yet-wired stubs; reach for the imported one instead.

Swap in `postgres://postgres:pass@localhost/lunaris` to run against Postgres
— the URL scheme is the only difference. See
[Choosing a Backend](../operations/backends.md).

## The wire shape

Episodes, forget requests, and hits cross the FFI as plain JavaScript
**objects** (deep-converted to/from the Rust structs). For the bare
`handle.ingest(obj)` path you build the `bt` bi-temporal stamp and the
26-char Crockford-base32 ULID `id` by hand, as in the quickstart.

For multi-agent partitioning the v0.2 surface also ships the typed
ergonomics — `Scope`, `EpisodeBuilder`, `ScopedLunaris`, and `lunarisScoped`
are all exported:

```typescript
import { Scope, EpisodeBuilder, lunarisScoped } from "@pilotspace/lunaris";

const scoped = lunarisScoped(handle, Scope.new("acme.agent-1"));   // ScopedLunaris
const lsn = await scoped.ingest(
  new EpisodeBuilder("notes", "Lunaris ingest via the typed builder.")
);
const hits = await scoped.recall("what did agent-1 note?");        // scope-pinned recall
```

`Scope.new("…")` validates against `[A-Za-z0-9_\-.]{1,128}` and throws on a
bad string; `EpisodeBuilder` mirrors the Rust builder (`new EpisodeBuilder(source, content)`,
then `.metadata(obj)` / `.tRef(iso8601)`), and its terminal conversion is crate-private, so only
`ScopedLunaris.ingest` can mint the scoped episode. The cross-language parity
test catches any divergence between the Python and TS surfaces.

## The retrieval DSL

`Vector`, `Keyword`, `Graph` compose; camelCase aliases (`fuseRrf`, `asOf`)
and the `filter(pred)` / `filterStr(s)` split match the JS idiom. A terminal
`.execute()` collapses the plan into a single FFI call:

```typescript
import { Keyword } from "@pilotspace/lunaris";

const hits = await handle
  .recall()                              // seeds a builder with default root Vector("chunks", 30)
  .and(Keyword.bm25("chunks", 30))
  .fuseRrf(60)                           // Reciprocal Rank Fusion, k=60
  .top(5)
  .execute();                            // takes no arguments in the v0.2 TS DSL
// hits is Hit[]; each Hit carries content, source, score, rawScore,
// validTime, sysTime, degraded (boolean), rerankApplied, sourceOp.
```

`.execute()` takes no arguments — the plan tree collapses to the
`index` / `k` / optional `filter` / `as_of_ms` knobs the `recallSimpleExecute`
FFI accepts (mirrors the Python side); a query-text setter on the builder is a
follow-up. Time-travel is one combinator (`asOf`). See
[The Retrieval DSL](../guides/retrieval-dsl.md) for the full operator set.

## Pipeline toggles (three surfaces)

`GraphPipeline` and `ConsolidatorPipeline` default OFF. Flip them at code,
env, or config; resolution order is **code > env > config** — code wins.

```typescript
// code surface
handle.graphPipeline.enable();
handle.consolidatorPipeline.disable();

// config surface — opts object on the ergonomic open() wrapper
const handle = await open(url, {
  graphPipeline: { enabled: true },
  consolidatorPipeline: { enabled: false },
});
```

```bash
# env surface — read at open() time
export LUNARIS_GRAPH_ENABLED=1
export LUNARIS_CONSOLIDATE_ENABLED=0
```

See [Consolidation & Verification](../guides/consolidate-verify.md) and
[The Graph Pipeline](../guides/graph.md).

## Embedder / reranker config

Override the env-driven default from code via `EmbedderConfig` /
`RerankerConfig`, surfaced as a chainable `withEmbedder` / `withReranker`
extension on the `Lunaris` class (camelCase opts bags):

```typescript
import { Lunaris, EmbedderConfig, RerankerConfig } from "@pilotspace/lunaris";

const emb = EmbedderConfig.fastembed({ cacheDir: "/var/cache/lunaris/fastembed", execution: "coreml" });
const rer = RerankerConfig.fastembed({ cacheDir: "/var/cache/lunaris/fastembed-reranker" });
const handle = (await Lunaris.open("moon://127.0.0.1:6380")).withEmbedder(emb).withReranker(rer);

// Latency floor — disable the cross-encoder rescoring pass:
const fallback = (await Lunaris.open(url)).withEmbedder(emb).withReranker(RerankerConfig.noop());
```

Factories (mirroring the Python surface, camelCase fields):
`EmbedderConfig.fastembed({ cacheDir, execution, showDownloadProgress })`,
`EmbedderConfig.ollama({ endpoint, model, dim })`,
`EmbedderConfig.fromOnnxBytes({ onnxBytes, tokenizerBytes, dim, pooling, execution })`,
`EmbedderConfig.fromOnnxPath({ onnxPath, tokenizerPath, dim, pooling, execution })`.
`RerankerConfig` ships `fastembed(...)` + `noop()` today.

Notes:

- The fastembed presets fetch weights from HF Hub on first use — point
  `cacheDir` (or `LUNARIS_FASTEMBED_CACHE_DIR`) at a writable path with
  ~1 GB free; one-time per host.
- BYO ONNX models **must** match the declared `dim` — the SDK wraps them in
  `DimValidatingEmbedder`, which raises a `LunarisError` on the first batch
  if they don't, rather than corrupting your vector index.
- The TS parser **warns and falls back to CPU** on an unknown / unavailable
  `execution` value (Python errors instead) — accelerator features
  (`fastembed-coreml` / `-cuda`) still need a build with that feature.
- **FFI cliff:** you cannot implement the Rust `Embedder` / `Reranker` trait
  from TypeScript — per-call FFI callbacks would be too slow for the hot
  path. Roll-your-own backends are Rust-crate-only.

## Async discipline

napi-rs 3.x's `tokio_rt` feature routes `#[napi] pub async fn` through the
shared tokio runtime, so every method that returns a `Promise` is a real
awaitable. The "never hold a `parking_lot::RwLock` across `.await`" invariant
is enforced on the Rust umbrella side; the TS host crate's emitted wrappers
take no locks themselves. Practical notes:

- `await handle.ingest(...)` / `await handle.recall()...execute()` are
  ordinary Promises — `await` them; don't block the event loop on them.
- The `lunaris` addon is not part of `cargo test --workspace` (it's a
  `cdylib`) — test TS code with `napi build` + `vitest`, or via
  `scripts/sdk-real-evidence.sh`.
- `conformanceFixtureEpisodes` / `scanKvPrefix` are only exported behind the
  `bindings-it` Cargo feature (per-driver parity tests) — production builds
  ship without them.

## Troubleshooting

- **`undefined symbol: napi_get_value_*` at load time** — your Node runtime
  is older than NAPI 8 (Node 18 or lower). Upgrade to Node 20 LTS+.
- **`fastembed: failed to fetch model …`** — first-call download is hitting
  the network; pre-populate `cacheDir`, ensure write access + ~1 GB free,
  confirm outbound HTTPS to `huggingface.co`.
- **`embedding-gemma weights missing …`** — the in-process embedder needs
  weights; download them or use the Ollama embedder.

## See also

- [Python SDK](./python.md) — the parallel surface
- [The Retrieval DSL](../guides/retrieval-dsl.md)
- [Configuration Reference](../reference/configuration.md)
- `crates/lunaris-ts/` — the binding crate
