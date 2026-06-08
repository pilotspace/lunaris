# Lunaris

**Sub-25 ms recall over millions of bi-temporal facts, with provable
atomicity and a graph that's opt-in.**

Lunaris is a production-grade **agent-memory engine** written in pure Rust,
with first-class **Python** (`pip install lunaris`) and **TypeScript**
(`npm i @pilotspace/lunaris`) SDKs generated from the same source of truth.

You feed it raw observations — chat turns, documents, tool outputs — as
`Episode`s. It chunks, embeds, and (optionally) extracts entities, relations,
and facts using a small local LLM, then stores everything in a bi-temporal
MVCC store backed by **Postgres** (the portable default) or **Moon** (a
high-performance Redis-compatible substrate). Agents query it through a
composable retrieval DSL that fuses semantic search, graph traversal, and
BM25 keyword lookup, with an optional cross-encoder rerank pass on top.

```rust
use lunaris::{EpisodeBuilder, Lunaris, Query, Scope};

let lunaris = Lunaris::open("postgres://lunaris@localhost/lunaris").await?;
let scope   = Scope::new("acme.agent-1")?;
let scoped  = lunaris.scoped(scope);

let lsn  = scoped.ingest(EpisodeBuilder::new("user-msg", "Alice loves chocolate.")).await?;
let hits = scoped.recall(Query::text("what does Alice like?")).await?;
```

Want a hybrid plan (vector + BM25, fused, reranked)? Compose it with the
[retrieval DSL](./guides/retrieval-dsl.md):

```rust
use lunaris::{Keyword, Query, Vector};

let hits = scoped
    .dsl()
    .with_root(Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(5))
    .execute(Query::text("what does Alice like?"))
    .await?;
```

## The three moats

Three properties define what Lunaris **is**. Every commit is reviewed against
them; any feature that weakens any of the three is rejected.

| Moat | What it means | Where enforced |
|---|---|---|
| **Sub-25 ms p50 recall** | No LLM on the recall hot path. Cross-encoder rerankers stay sub-30 ms. | `cargo bench --bench recall_hot_path` + perf smoke in CI |
| **Single `atomic_write` per ingest** | All-or-nothing commit across vector, KV, BM25, queue. Fan-out architectures (Mem0, Zep) can't make this guarantee. | `tests/ingest_pipeline.rs::single_atomic_write_call` + CI grep gate |
| **Bi-temporal MVCC + HLC** | `BiTemporal { valid, sys }` on every primitive. "What did the agent know at time T" is a query, not a rebuild. | Required field on `Episode`, `Chunk`, `Entity`, `Fact`, `Relation`, `Community` |

If everything else fails, that performance + correctness contract must hold —
it's what differentiates Lunaris from Mem0, Zep, and Cognee. See
[Why Lunaris](./getting-started/why-lunaris.md) for the honest "use a
different tool when…" criteria.

## How this book is organized

- **[Getting Started](./getting-started/installation.md)** — install, a
  10-minute quickstart, and the core concepts (episodes, scope, bi-temporal
  MVCC, the atomic write).
- **[Guides](./guides/ingest.md)** — one chapter per capability: ingest, the
  retrieval DSL, forget, the opt-in graph pipeline, consolidation &
  verification, multi-agent scoping.
- **[Cookbook](./cookbook/index.md)** — the built-in recipe types
  (chat agents, document corpora, Slack/email archives, code-repo memory,
  timelines) as copy-pasteable how-tos.
- **[Reference](./reference/configuration.md)** — the exhaustive
  configuration reference (every feature flag and `LUNARIS_*` env var), the
  generated API docs, and the error taxonomy.
- **[Operations](./operations/server.md)** — running the HTTP server,
  choosing a backend, durability & recovery.
- **[SDKs](./sdk/python.md)** — Python and TypeScript surface notes.
- **[Migrating From](./migrating/mem0.md)** — Mem0 / Zep / Cognee mapping
  tables.
- **[Protocol](./protocol/memoryprotocol-0.1.md)** — the MemoryProtocol 0.1
  HTTP/SSE wire spec and its conformance suite.

> **Source of truth.** Where a claim in this book disagrees with the Rust
> source, the source wins. Many pages carry `path:line` cross-references back
> into the crates; the generated [API reference](./reference/api.md) is built
> from `cargo doc` on every release.
