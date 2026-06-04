# API Reference (docs.rs)

**The exhaustive, type-level API reference is the generated rustdoc** — this
book covers the *how* and *why*; rustdoc covers every type, trait, method, and
signature.

- Online (per release): `https://docs.rs/lunaris-memory` (the umbrella crate
  is published as `lunaris-memory`; its library name is `lunaris`) — built
  from `cargo doc` with the documentation feature set (see each crate's
  `[package.metadata.docs.rs]`).
- Locally: `cargo doc --workspace --no-deps --open` (or `make docs-rust`).

## The `lunaris` umbrella crate

`lunaris` re-exports the surface you normally touch. For the common subset,
glob-import the prelude:

```rust
use lunaris::prelude::*;
// → Lunaris, ScopedLunaris, Scope, EpisodeBuilder, Query, Hit,
//   Vector, Keyword, Graph, Tree, RetrievalBuilder, ForgetTarget, ScopeSpec,
//   LunarisError, Embedder, HlcClock,
//   Reranker/NoopReranker, Extractor/NoopExtractor,
//   Verifier/NoopVerifier, Consolidator/NoopConsolidator
```

The prelude is intentionally small. Reach into the full re-export list (or the
member crates directly) for everything else — extractor/verifier backend
structs, the storage concretes (`MoonStorage`, `PostgresStorage`), the recipe
types, `init_logging`, the pipeline handles, etc.

## Crate map

| Crate | Role |
|---|---|
| `lunaris` | Umbrella: `Lunaris` / `ScopedLunaris` handles, `open()` URL dispatcher, the ingest hot path, re-exports, `prelude` |
| `lunaris-core` | Shared primitives (`Episode`/`Chunk`/`Entity`/`Fact`/`Relation`/`Community`), `StoragePort` trait, HLC clock, `Scope` newtype, `keyspace`, error taxonomy, circuit breaker |
| `lunaris-ingest` | Markdown chunker + batched embedder driver + the single `atomic_write` |
| `lunaris-retrieve` | The retrieval DSL (`Vector`/`Keyword`/`Graph`/`Tree`, combinators, RRF fusion, rerank, fallback), `tower::Service`-shaped |
| `lunaris-extract` | Entity/relation/fact extractor (candle / Ollama / cloud-API) + validator |
| `lunaris-consolidate` | ACT-R consolidator (Anderson 1996; Leiden communities) — opt-in |
| `lunaris-verify` | Slow-path arbitration verifier + MVCC supersede writer — opt-in |
| `lunaris-embed` | `Embedder` impls (fastembed / candle / Ollama) |
| `lunaris-rerank` | Cross-encoder reranker (BGE-Reranker-v2-m3) + `NoopReranker` |
| `lunaris-storage-moon` | `StoragePort` on a Redis-compatible substrate (native `FT.*`, `GRAPH.QUERY`, MQ, RRF) |
| `lunaris-storage-postgres` | `StoragePort` on Postgres + pgvector + Apache AGE + pgmq (RLS-enforced isolation) |
| `lunaris-server` | HTTP + SSE MemoryProtocol 0.1 server (axum) |
| `lunaris-recipes` | Recipe primitives + conversational / documentary wrappers (see [Cookbook](../cookbook/index.md)) |
| `lunaris-py` / `lunaris-ts` | PyO3 / napi-rs bindings (see [SDKs](../sdk/python.md)) |

> `lunaris-codegen`, `lunaris-conformance`, and `lunaris-bench` are internal
> tooling crates — not part of the public API.

> **Publishing note.** The umbrella crate is published as **`lunaris-memory`**
> (the bare `lunaris` name on crates.io is taken; the *library* name stays
> `lunaris`, so the import path is unchanged). `lunaris-storage-moon` depends
> on `moondb` from crates.io (version-pinned), so the workspace is
> crates.io-publishable; the `[package.metadata.docs.rs]` blocks are wired so
> `docs.rs` builds the right feature set.
