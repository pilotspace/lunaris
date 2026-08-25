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

```rust,no_run
# use lunaris::{EpisodeBuilder, ForgetTarget, Graph, Hit, Keyword, Lunaris, Query, Scope, ScopeSpec, Tree, Vector};
# async fn demo() -> Result<(), lunaris::LunarisError> {
use lunaris::prelude::*;
// → Lunaris, ScopedLunaris, Scope, EpisodeBuilder, Query, Hit,
//   Vector, Keyword, Graph, Tree, RetrievalBuilder, ForgetTarget, ScopeSpec,
//   LunarisError, Embedder, HlcClock,
//   Reranker/NoopReranker, Extractor/NoopExtractor,
//   Verifier/NoopVerifier, Consolidator/NoopConsolidator
# Ok(())
# }
```

The prelude is intentionally small. Reach into the full re-export list (or the
member crates directly) for everything else — extractor/verifier backend
structs, the storage concrete (`MoonStorage`), the recipe
types, `init_logging`, the pipeline handles, etc.

## Crate map

| Crate | Role |
|---|---|
| `lunaris` | Umbrella: `Lunaris` / `ScopedLunaris` handles, `open()` URL dispatcher, the ingest hot path, re-exports, `prelude` |
| `lunaris-core` | Shared primitives (`Episode`/`Chunk`/`Entity`/`Fact`/`Relation`/`Community`), `StoragePort` trait, HLC clock, `Scope` newtype, `keyspace`, error taxonomy, circuit breaker |
| `lunaris-ingest` | Markdown chunker + batched embedder driver + the single `atomic_write` |
| `lunaris-retrieve` | The retrieval DSL (`Vector`/`Keyword`/`Graph`/`Tree`, combinators, RRF fusion, rerank, fallback), `tower::Service`-shaped |
| `lunaris-extract` | Entity/relation/fact extractor (remote-only: Ollama / cloud-API providers) + validator |
| `lunaris-consolidate` | ACT-R consolidator (Anderson 1996; Leiden communities) — opt-in |
| `lunaris-verify` | Slow-path arbitration verifier (remote-only providers) + MVCC supersede writer — opt-in |
| `lunaris-llamacpp` | `Embedder` + `Reranker` impls — in-process llama.cpp (`LlamaCppEmbedder`: granite-r2 Q4_K_M GGUF; `LlamaCppReranker`: bge-reranker-v2-m3 Q5_K_M GGUF); default-enabled `llamacpp` feature on the umbrella |
| `lunaris-embed-remote` | `Embedder` impl — Ollama HTTP escape hatch (`--features embed-remote`); resolves after the llama.cpp step |
| `lunaris-rerank` | `Reranker` trait + `NoopReranker` (the cross-encoder impl lives in `lunaris-llamacpp`) |
| `lunaris-storage-moon` | `StoragePort` on a Redis-compatible substrate (native `FT.*`, `GRAPH.QUERY`, MQ, RRF) |
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
