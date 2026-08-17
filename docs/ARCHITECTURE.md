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
| **L3 — Cognition** | `lunaris-ingest`, `lunaris-retrieve`, `lunaris-extract`, `lunaris-consolidate`, `lunaris-verify` + ML runtimes `lunaris-llamacpp`, `lunaris-llm`, `lunaris-embed-remote` | Pipelines that turn raw observations into primitives, and queries into fused result sets. Embedder + reranker run on in-process llama.cpp (GGUF, static-linked FFI); extractor/verifier LLM slots are remote-only |
| **L2 — Contracts** | `lunaris-core` | The kernel: primitives, `BiTemporal`, HLC clocks, validated `Scope`, canonical keyspace, `StoragePort` / `KeywordPort` traits, capability negotiation, circuit breakers, audit |
| **L1 — Substrate** | `lunaris-storage-moon` | One trait, one backend. The Postgres portability proof and the SQLite zero-deps backend were deleted in 0.7.0 — see [`migration/0.6-to-0.7`](migration/0.6-to-0.7.md) |

Cross-cutting: `lunaris-conformance` (the port conformance suite — now a
single-backend contract test rather than a parity harness), `lunaris-bench`
(perf gates), `lunaris-codegen` (SDK generation).

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

## How a memory is structured

One ingested episode fans out into a small constellation of rows, all
minted under the canonical keyspace
`lunaris:{scope}:{kind}:{ulid}` (`lunaris_core::keyspace`) and all
committed by the same `atomic_write`:

```text
Episode  lunaris:{scope}:episode:{ulid}     source, raw content, metadata, bt
  └─ Chunk(s)  lunaris:{scope}:chunk:{ulid} text + heading_path + episode_id
       ├─ vector        768-d embedding — same HSET document
       ├─ BM25 payload  tokenized text — same FT index as the vector
       └─ bt stamp      [sys_from, sys_to) × [valid_from, valid_to)
  └─ (opt-in graph) Entity / Relation / Fact rows + GraphNode/GraphEdge
       in the per-scope named graph
  └─ Audit row + one __lunaris_consolidate__ queue message
```

Three structural decisions carry the recall story:

- **The chunk is the retrieval unit, the episode is the provenance
  unit.** Vector and BM25 hits return chunk ULIDs; hydration walks
  `chunk → episode_id → episode` to give every hit its source.
- **One document, two indices' worth of duty.** On Moon the chunk's
  embedding, its BM25-tokenized text, its `TAG` filter fields, and its
  bi-temporal stamp live in a single `HSET` document indexed by ONE
  per-scope `FT` index (`lunaris_{scope}_{kind}_idx`). There is no
  "sync the vector DB with the text index" job because there is
  nothing to sync.
- **Every row is bi-temporal — on the write side, on every backend.**
  The `bt` field records system time (when Lunaris learned it) and valid
  time (when it was true in the world) as two half-open intervals.
  `forget` and supersession close intervals rather than destroy rows.
  **As-of *reads* are lane-dependent**: `.as_of(ts)` resolves the
  search-side and graph-side cut natively (`FT.SEARCH AS_OF` /
  `GRAPH.QUERY VALID_AT`), but the KV hydration step behind it has no
  version chain to walk on Moon, so a historical KV pin is refused
  outright. The Postgres/SQLite backends that answered it were deleted in
  0.7.0, so this is now a flat limitation rather than a backend choice. See [Honest limits](#honest-limits-read-before-quoting-the-table-above).

## Where the milliseconds go — anatomy of a recall

The sub-25 ms contract is not one trick; it is the absence of four
round trips. A `vector.and(keyword).fuse_rrf(60)` recall spends its
budget like this:

| Stage | What happens | Why it's fast |
|---|---|---|
| 1. Query embed | granite-embedding-311m runs **in-process** on llama.cpp (Q4_K_M GGUF) | No HTTP hop to an embedding server. This is the single biggest win — see the 86 ms lesson below |
| 2. Hybrid search | ONE `FT.SEARCH` HYBRID round trip; Moon fuses vector KNN + BM25 with **native RRF** server-side (`RrfFusion::Moon`) | Fusion happens inside the engine that owns both indices — not N queries glued together in app code |
| 2a. Filters & time | `TAG` pre-filters (`@source:{...}`) and `AS_OF <ms>` resolve **inside** the same search command (PERF-MOON-01) | Filtering before scoring; the temporal cut never becomes an app-side post-filter |
| 3. Hydrate | Every hit's chunk row fetched **concurrently** (ordered `buffered(32)` fan-out, one `HMGET` per row); parent episodes fan out once per unique `episode_id` (`lunaris-retrieve/src/hydrate.rs`) | Concurrent requests pipeline over one multiplexed connection — k hydrations cost ~1 batch of round trips, not 2k serial ones. Since-deleted chunks are skipped, not errored |
| 4. Rerank (opt-in) | bge-reranker-v2-m3 cross-encoder, in-process | ~12 ms p50 budget; only pay it when you ask for it — `LUNARIS_RECALL_RERANK=1` (MCP + HTTP/SDK recall; the hook hot path never reranks) or an explicit `.rerank(..)` in the DSL |

**The 86 ms lesson.** The same 10k-document SQuAD harness
(`scripts/bench-squad-kb.py`) measured **p50 86 ms** when query
embedding went through an out-of-process Ollama HTTP server, and
**p50 10.3 ms / p99 20.8 ms** on the strict-replay path that removes
that hop (`scripts/ollama-replay-server.py` +
`scripts/precompute-embeds.py`). The engine's own search + hydrate
path was ~10 ms all along; the network hop to the embedder was ~75 ms
of pure overhead. That measurement is why v0.4 (N-03) moved embedding
in-process as the default — a property the v0.6 llama.cpp-only cutover
preserves (llama-cpp-2 is in-process FFI, static-linked) — so the
deployment matches the configuration the contract was proven on.

**Why a fan-out stack can't follow:** vector DB + text search + graph
DB + broker means (a) one network round trip *per lane* plus app-side
fusion, (b) no shared `TAG`/temporal pushdown — you over-fetch then
post-filter, and (c) no common snapshot, so the lanes can disagree
about what exists. Lunaris-on-Moon spends its entire budget inside one
process and one index.

## The Moon advantage map

Moon is our internal Redis-compatible substrate. The reason it's the
flagship backend is that each `StoragePort` method maps onto a Moon
*native* capability — Lunaris does not reimplement a vector index, a
BM25 scorer, a graph engine, or a transaction log on top of a dumb KV
store. (Source for each mapping: `crates/lunaris-storage-moon/src/`.)

![What Moon does natively, feature by feature](book/src/images/architecture/moon-feature-superpower.png)

| Lunaris needs | Moon native answer | What it buys you |
|---|---|---|
| All-or-nothing ingest commit | `TXN.BEGIN` … `TXN.COMMIT` server-side transaction spanning KV, vector, BM25, graph, audit, queue writes (`atomic.rs`) | The atomicity contract fan-out architectures can't make. One crash window: zero |
| Semantic search | `FT.SEARCH` KNN over auto-indexed `HSET` writes; no dimension cap (`vector.rs`, `client.rs`) | No sidecar vector DB. Writes are indexed by the same engine that stores them |
| Keyword search | `FT.SEARCH` BM25 scoring on the **same index** (`keyword.rs`) | No second text-search system. One index serves both retrieval modes |
| Hybrid fusion | Moon-side native RRF (`RrfFusion::Moon`) on HYBRID `FT.SEARCH` | Fusion happens in the engine, one round-trip, instead of N queries glued in app code |
| Time travel (search + graph only) | `FT.SEARCH … AS_OF <ms>` and `GRAPH.QUERY … VALID_AT <ms>` (`vector.rs`, `graph.rs`) | The temporal cut resolves inside the search command. **Not** KV: `read_as_of` at a past instant is refused on Moon — see [Honest limits](#honest-limits-read-before-quoting-the-table-above) |
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
| Contract holds on Moon v0.3.0 | **retrieval-only p50 3.1 ms / p99 3.6 ms** (3k-doc SQuAD train, Q4-GGUF granite embedder, live Moon — smaller corpus than the 10k baseline, see the caveat in the report) | p50 < 25 ms | [`docs/benchmarks/v0.7-moon-v030-rerun.md`](benchmarks/v0.7-moon-v030-rerun.md) |
| Navigate recall edge (graph-linked corpora only) | **plain 0.00 → nav 1.00 recall@5** on 2-hop graph-linked targets, +0.05 ms p50 | opt-in graph | [`docs/benchmarks/v0.7-moon-v030-rerun.md`](benchmarks/v0.7-moon-v030-rerun.md) |
| Flat tail at k=30 | **p50 6.0 ms / p99 6.2 ms** (was p50 12 / p99 97.3 before the hydration fan-out, MCP stdio, live Moon) | p99 inside the p50 contract | [`docs/benchmarks/v0.6-recall-fanout-ab.md`](benchmarks/v0.6-recall-fanout-ab.md) |
| One atomic write per ingest | exactly 1 `atomic_write` call site | INGEST-04 | `crates/lunaris-ingest/tests/ingest_pipeline.rs::single_atomic_write_call` + CI grep gate |
| Port contract | Moon passes the full port conformance suite | conformance | `crates/lunaris-conformance` |

## Honest limits (read before quoting the table above)

The advantage map is real, but it is not "bi-temporal everywhere":

- **As-of KV reads do not work on Moon, and now say so.** Moon stores
  Lunaris rows as plain hashes; `HGET`/`HMGET` accept no `AS_OF` clause
  and an overwrite destroys the prior value, so there is no version to
  return. Only `FT.SEARCH AS_OF` and `GRAPH.QUERY VALID_AT` are
  temporal on Moon.
  Since 0.6.2, `read_as_of` / `scan_range` on the Moon backend **refuse**
  a historical pin with `StorageError::NotSupported` (HTTP `501
  not_supported`) instead of silently answering with present-time data
  — which is what they did before, making `GET /v1/snapshot/{lsn}` and
  `POST /v1/recall {as_of: <past>}` return fabricated history.
  Latest-state reads (the entire production hot path) are unaffected.
  The backend declares this via `StoragePort::supports_historical_kv_reads()`
  — `false`, and since 0.7.0 there is no backend that returns `true`. Pinned
  by
  `lunaris-storage-moon/tests/read_as_of_historical.rs` and the
  non-skipping `read_as_of::historical_pin_is_explicit` conformance
  test. The upstream path to closing it is Moon's `TemporalKvIndex`
  (`record`/`get_at`), which has no production call sites yet.
- **`FT.CREATE` schemas are sticky.** Moon will not resize or reshape an
  existing index in place; changing embedder dimension requires
  `FT.DROPINDEX` + re-ingest (`client.rs` fails fast on mismatch).
- **Graph parameters are inlined.** Moon's `GRAPH.QUERY` ignores
  `--params`, so the adapter renders literals; property filters use
  `WHERE` clauses, never inline `{id: …}` maps (`atomic.rs`, fixed in
  `adbac2d`).
- **There is no second backend to fall back to.** Through 0.6.x, Postgres
  (tsvector BM25, pgvector, RLS-enforced scope) and SQLite took different
  routes to the same contract at different performance points. 0.7.0 deleted
  both: one substrate to test, tune, and operate. The cost is that Moon's
  gaps — the two above — are now the engine's gaps, and that running Lunaris
  means running Moon. See
  [`operations/backends`](book/src/operations/backends.md).

## Where to go next

- Evaluator one-pager: [`POSITIONING.md`](POSITIONING.md)
- Public book chapter (diagram-first version of this page):
  `docs/book/src/getting-started/architecture.md`
- Retrieval DSL guide: `docs/book/src/guides/retrieval-dsl.md`
- Durability story: [`durability.md`](durability.md)
