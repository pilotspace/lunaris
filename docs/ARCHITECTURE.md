# Lunaris architecture — the layered view

This is the canonical "how Lunaris is built" page. It complements
[`POSITIONING.md`](POSITIONING.md) (should I use it?) by answering *how
does it work, and why is Moon the flagship substrate?* Every claim is
anchored to a code path or a published benchmark — no marketing-only
statements.

![Lunaris layered architecture](book/src/images/architecture/lunaris-layers.png)

## The five layers

Lunaris is a 27-crate Rust workspace organized as strict layers. Each
layer only depends on the contracts of the layer below it — backends are
swappable because the engine never sees a concrete database.

| Layer | Crates | Responsibility |
|---|---|---|
| **L5 — Surface** | `lunaris-server` (axum HTTP + SSE), `lunaris-py` (PyO3), `lunaris-ts` (napi-rs), `lunaris-mcp` (+ `lunaris-mcp-npm` / `lunaris-mcp-py` dist), `lunaris-hook`, `lunaris-recipes` | How agents reach the engine: HTTP, Python, TypeScript, MCP tools, proactive capture hooks, prebuilt recipes |
| **L4 — Engine facade** | `lunaris` (umbrella) | One handle, whole engine: `open()`, `ingest()`, `recall()`, `forget()`, `snapshot()`, `structured_ingest()`, plus the opt-in graph / consolidator / verifier pipelines |
| **L3 — Cognition** | `lunaris-ingest`, `lunaris-retrieve`, `lunaris-extract`, `lunaris-consolidate`, `lunaris-verify` + ML runtimes `lunaris-embed-native`, `lunaris-rerank-native`, `lunaris-llm`, `lunaris-embed-remote` | Pipelines that turn raw observations into primitives, and queries into fused result sets. All models run on `candle` (CPU), in-process |
| **L2 — Contracts** | `lunaris-core` | The kernel: primitives, `BiTemporal`, HLC clocks, validated `Scope`, canonical keyspace, `StoragePort` / `KeywordPort` traits, capability negotiation, circuit breakers, audit |
| **L1 — Substrate** | `lunaris-storage-moon` (flagship), `lunaris-storage-postgres` (portability proof), `lunaris-storage-embedded` (SQLite, zero-deps) | One trait, three backends. Moon first by design |

Cross-cutting: `lunaris-conformance` (every backend passes the same port
conformance suite), `lunaris-bench` (perf gates), `lunaris-codegen`
(SDK generation).

### The port is the architecture

`StoragePort` (`crates/lunaris-core/src/storage/port.rs`) is 13 methods:

```text
atomic_write · vector_search · graph_traverse · scan_range · read_as_of
publish · subscribe · queue_depth · list_scopes · invalidate_range
capabilities · lookup_by_dedupe_key · insert_dedupe_key
```

plus `KeywordPort::keyword_search` for BM25. Everything above L2 is
written against these signatures. `capabilities()` lets a backend
declare what it can do natively (temporal reads, native RRF, native
queue, vector dimension) and the retrieval operators degrade gracefully
(`lunaris-retrieve/src/operators/degraded.rs`) when a capability is
absent — the engine adapts to the substrate instead of assuming it.

![Ingest to recall pipeline](book/src/images/architecture/lunaris-pipeline.png)

## The data path

**Ingest** (left to right): an observation enters via any L5 surface →
`EpisodeBuilder` shapes it → chunking → single-pass embedding
(granite-embedding-311m, 768-d) → optional extraction (entities,
relations, facts via the local LLM) → **everything fans into ONE
`WriteOp` vector and exactly one `atomic_write` call**
(`crates/lunaris-ingest/src/pipeline.rs`, invariant INGEST-04, enforced
by a CI grep gate). Either the episode, its chunks, vectors, BM25
payload, graph nodes/edges, audit row, and queue message all commit — or
none of them do.

**Recall** (right to left): a typed DSL expression
(`crates/lunaris-retrieve/src/builder.rs`) compiles into operators —
`vector`, `keyword`, `graph`, `recency`, `tree` (RAPTOR) — fused by RRF
and optionally reranked by a cross-encoder
(bge-reranker-v2-m3). `.as_of(ts)` pushes the temporal cut down into the
substrate query itself, not a post-filter in application code.

## The Moon advantage map

Moon is our internal Redis-compatible substrate. The reason it's the
flagship backend is that each `StoragePort` method maps onto a Moon
*native* capability — Lunaris does not reimplement a vector index, a
BM25 scorer, a graph engine, or a transaction log on top of a dumb KV
store. (Source for each mapping: `crates/lunaris-storage-moon/src/`.)

| Lunaris needs | Moon native answer | What it buys you |
|---|---|---|
| All-or-nothing ingest commit | `TXN.BEGIN` … `TXN.COMMIT` server-side transaction spanning KV, vector, BM25, graph, audit, queue writes (`atomic.rs`) | The atomicity contract fan-out architectures can't make. One crash window: zero |
| Semantic search | `FT.SEARCH` KNN over auto-indexed `HSET` writes; no dimension cap (`vector.rs`, `client.rs`) | No sidecar vector DB. Writes are indexed by the same engine that stores them |
| Keyword search | `FT.SEARCH` BM25 scoring on the **same index** (`keyword.rs`) | No second text-search system. One index serves both retrieval modes |
| Hybrid fusion | Moon-side native RRF (`RrfFusion::Moon`) on HYBRID `FT.SEARCH` | Fusion happens in the engine, one round-trip, instead of N queries glued in app code |
| Time travel | `FT.SEARCH … AS_OF <ms>` and `GRAPH.QUERY … VALID_AT <ms>` (`vector.rs`, `graph.rs`) | "What did the agent believe at time T?" resolves inside the search command |
| Graph memory | `GRAPH.QUERY` (Cypher), one named graph **per scope** (`graph.rs`, `keyspace.rs`) | Opt-in graph traversal without operating Neo4j; tenants can't see each other's graph by construction |
| Bulk invalidation | `FT.INVALIDATE_RANGE` (`invalidate.rs`) | GDPR-grade forget over a time range without a scan-and-delete loop |
| Server-side filters | `TAG` schema fields → `@source:{value}` resolves in Moon (`atomic.rs`, PERF-MOON-01) | Filtering before scoring, not after fetching |
| Multi-tenant isolation | Per-scope FT indices `lunaris_{scope}_{kind}_idx` + per-scope graph (`scopes.rs`) | Isolation at the index level, beneath the type-level `Scope` validation |
| Async work | Native queue + pub/sub (`queue.rs`) | Consolidation/verification pipelines need no external broker |
| Durability | AOF + `BGREWRITEAOF` recovery (see [`durability.md`](durability.md)) | Recoverable state, verified by `scripts/test-recovery.py` |

**The collapsed-stack consequence:** a typical agent-memory deployment
runs a vector DB + a graph DB + a relational store + a message broker.
Lunaris on Moon is **one process** providing all four lanes — and one
transaction boundary across them, which a multi-system stack cannot
offer at any price.

![Moon vs the 3-database stack](book/src/images/architecture/moon-vs-stack.png)

## The numbers that anchor the claims

| Claim | Measured | Contract | Proof |
|---|---|---|---|
| Sub-25 ms recall | **p50 10.3 ms / p99 20.8 ms** (strict replay, 10k-doc corpus, live Moon) | p50 < 25 ms | [`docs/benchmarks/v0.2.x/README.md`](benchmarks/v0.2.x/README.md) |
| One atomic write per ingest | exactly 1 `atomic_write` call site | INGEST-04 | `crates/lunaris-ingest/tests/ingest_pipeline.rs::single_atomic_write_call` + CI grep gate |
| Backend parity | Moon + Postgres + SQLite pass the same suite | conformance | `crates/lunaris-conformance` |

## Honest limits (read before quoting the table above)

The advantage map is real, but it is not "bi-temporal everywhere":

- **Plain KV reads are not temporal on Moon.** `HGET` ignores `AS_OF`;
  only `FT.SEARCH AS_OF` and `GRAPH.QUERY VALID_AT` are temporal.
  Lunaris stores an explicit `bt` field on KV rows and the engine
  applies the temporal cut for `read_as_of` (`kv.rs`,
  `capabilities()` reports `bi_temporal_native: false`).
- **`FT.CREATE` schemas are sticky.** Moon will not resize or reshape an
  existing index in place; changing embedder dimension requires
  `FT.DROPINDEX` + re-ingest (`client.rs` fails fast on mismatch).
- **Graph parameters are inlined.** Moon's `GRAPH.QUERY` ignores
  `--params`, so the adapter renders literals; property filters use
  `WHERE` clauses, never inline `{id: …}` maps (`atomic.rs`, fixed in
  `adbac2d`).
- **Postgres and SQLite take different routes to the same contract** —
  e.g. tsvector BM25, pgvector, RLS-enforced scope — at different
  performance points. Moon is the latency flagship; the others are the
  portability proof. See
  [`operations/backends`](book/src/operations/backends.md) for the
  decision guide.

## Where to go next

- Evaluator one-pager: [`POSITIONING.md`](POSITIONING.md)
- Public book chapter (diagram-first version of this page):
  `docs/book/src/getting-started/architecture.md`
- Retrieval DSL guide: `docs/book/src/guides/retrieval-dsl.md`
- Durability story: [`durability.md`](durability.md)
