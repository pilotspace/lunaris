# Lunaris Architecture Overview

Lunaris is a production-grade agent memory engine designed to take raw observations from agent harnesses, extract structured primitives, and store them in a bi-temporal MVCC store backed by Moon and Postgres. The system is composable: agents query it through a retrieval DSL that fuses semantic search, graph traversal, and BM25 keyword lookup. The audience for this document is engineers building on top of Lunaris: people who need to understand the storage contract, the ingest hot path, the recall hot path, and the bi-temporal semantics.

## Six Primitives

Lunaris models memory as six bi-temporal typed primitives. Each primitive carries a `BiTemporal { valid, sys }` stamp from a shared HLC clock so that every read can be reproduced as of any past timestamp. The primitives are deliberately small in count: more sacrifices interoperability with downstream graph reasoning frameworks, fewer sacrifices the ability to model contradictions and corrections cleanly. The six are Episode, Chunk, Entity, Relation, Fact, and Community.

### Episode

An Episode is a raw observation: a chat message, a document upload, a tool invocation result. Episodes are immutable snapshots of input. They carry an opaque source string (e.g., `helios:fs/notes.md`), an optional reference time `t_ref` for backdating historical events, and the original content. Episodes are the unit of ingest.

### Chunk

A Chunk is a sub-segment of an Episode produced by the markdown-aware chunker. Chunks carry a 768-dimensional embedding from EmbeddingGemma, the heading path lineage, and an overlap tail with the previous chunk. The chunker targets 500 tokens per chunk with 100 tokens of overlap, balancing recall granularity against embedding compute cost.

### Entity

An Entity is a named thing extracted from an Episode by the slow-path graph extractor. Entities have a name, optional aliases, an entity type (Person, Place, Organization, Concept), an embedding, and a confidence score. The graph pipeline is opt-in via `GraphPipeline::enable()` and defaults to off because most retrieval workloads do not need it.

### Relation

A Relation connects two Entities with a typed predicate. Relations carry provenance pointers back to the Episode they were extracted from so that downstream consumers can audit the chain of evidence.

### Fact

A Fact is a subject-predicate-object triple with full natural-language `fact_text`. Facts have an activation score that decays over time per the ACT-R consolidation model. High-activation Facts get promoted into the long-term store; low-activation Facts get archived.

### Community

A Community is a clustering of related Entities produced by the Leiden community detection algorithm. Each Community carries a summary and a summary embedding so that recall against community-level summaries is fast.

## Storage Contracts

The `StoragePort` trait is the only abstraction in Lunaris with strict guarantees. All eight methods are documented inline with their pre- and post-conditions, and the conformance harness runs identical tests against both backends to prove identity. The trait shape is locked verbatim against the blueprint with two pre-approved deviations for object-safety.

### Atomic Write

`atomic_write(&[WriteOp])` either commits all ops or none. Backends translate each `WriteOp` variant to their native command. On Moon: `TXN.BEGIN` / per-op (`HSET` / `FT.UPSERT` / `GRAPH.QUERY MERGE`) / `TXN.COMMIT`. On Postgres: `BEGIN` / per-op (`INSERT lunaris_kv` / `INSERT chunks` / `SELECT cypher()`) / `COMMIT`. The single-call invariant for ingest is non-negotiable: one Episode plus all its Chunks fan into one batch.

### Vector Search

`vector_search(index, query, k, filter, as_of, rerank)` returns the top-k vectors by cosine similarity with optional bi-temporal snapshot semantics and optional native rerank. Backends without native rerank set `rerank_applied=false` in each hit and the retriever can apply rerank downstream or skip per `degraded_fallback`. The filter algebra is intentionally small: `Eq`, `StartsWith`, `And`, `Or` cover the recipes shipped in v0.

### Read As Of

`read_as_of(key, as_of)` performs an MVCC point read at a specific HLC. This is the foundation for the time-travel feature: the agent can ask "what did you know at time T" and Lunaris returns the snapshot. Postgres emulates this with a four-clause bi-temporal predicate; Moon uses the native `TEMPORAL.SNAPSHOT_AT` command.

## Ingest Hot Path

The ingest hot path takes an Episode and lands typed Chunks plus their embeddings into one atomic write. The flow is straightforward but every step has a latency budget that adds up.

### Chunker

The markdown chunker walks `pulldown-cmark` events, maintains a heading stack, and emits chunks every time the accumulated token count crosses the 500-token target. Token counts are estimated as `whitespace_words * 1.3` for v0; full BPE round-trip via `tokenizers` is a v1 polish. The chunker is pure (no IO) so it never errors.

### Embedder

The embedder is the `Embedder` trait from `lunaris-core`. The default v0 backend is `CandleEmbeddingGemma` (768d, EmbeddingGemma 300M, token-embedding mean-pool with L2 normalization). The alt backend is `OllamaEmbedder` for the latency-budget escape hatch. Batches of 32 chunks per `embed_batch` call; on batch failure, fall back to per-chunk.

### Atomic Fan-Out

The pipeline assembles one `Vec<WriteOp>` containing one `KvPut` for the Episode and per-chunk pairs of `KvPut` (chunk JSON) plus `VectorUpsert` (embedding plus metadata). The single `atomic_write` call ships all of them together. The Phase 1 backends guarantee all-or-nothing semantics so a partial failure does not leave orphan rows.

## Recall Hot Path

The recall hot path is implemented as a tower service so it composes cleanly with rate-limit, retry, timeout, and tracing layers from the Tower ecosystem. The retrieval DSL exposes Vector and Keyword operators, the and / or / then combinators for parallel and sequential composition, the fuse_rrf reciprocal rank fusion operator, and the rerank operator backed by the bge-reranker cross-encoder.

### Vector Operator

Vector::new(index, k) embeds the query once via the cached embedder and runs `vector_search` on the storage backend. The embedding cache is per-query so chained operators do not re-embed. Phase 2 ships this operator end-to-end on both Moon and Postgres backends.

### Keyword Operator

Keyword::bm25(index, k) runs `FT.AGGREGATE` BM25 path on Moon and `tsvector` plus `ts_rank_cd` fallback on Postgres. Both produce `Vec<KeywordHit>` with normalized scores. The Postgres path requires a generated `tsvector` column with a GIN index per primitive table that has text content.

### Fuse RRF

fuse_rrf(k) reciprocal rank fusion across heterogeneous result sets uses the standard formula `score = sum 1/(k + rank_i)` with `k = 60` default. When the underlying backend reports `native_rrf=true` (Moon backend after Phase 1.5 retrofit), the planner can opt into Moon-native fusion via `text().hybrid_search` for a single-round-trip path; otherwise it folds client-side.

### Rerank

rerank(model) loads `bge-reranker-v2-m3` via candle, reads the cross-encoder logits for each (query, candidate) pair, and re-ranks the top-30 down to top-k. The reranker is optional: if the model file is missing, the operator degrades to a no-op and the pipeline continues with the raw RRF order.

## Bi-Temporal Semantics

Lunaris models time on two axes per the SQL:2011 PERIOD specification: `valid` time captures when a fact was true in the world, and `sys` time captures when the system observed or recorded the fact. Half-open intervals `[from, to)` with `to=None` meaning "still valid" or "still recorded". This separation lets Lunaris handle backdated corrections without losing the audit trail of what the system originally believed.

### AS_OF Queries

Every read path on `StoragePort` accepts an optional `as_of: Hlc` parameter. When set, the read returns the snapshot visible at that time on both axes. This is the primitive for time-travel queries: the agent asks "what did you know at time T" and Lunaris returns the historical view.

### Invalidations

When a fact is corrected, the original row is not deleted: instead, its `valid_to` (or `sys_to`) field is set to the invalidation HLC. The new row is inserted with a fresh `valid_from`. Reads at any prior HLC continue to see the original; reads at the invalidation HLC or later see the correction. This is true MVCC: no destructive overwrites, ever.

## Performance Targets

The performance targets in v0 are aggressive but measured. Sub-25ms recall over millions of bi-temporal facts on Moon is the headline. Postgres is allowed up to 60ms because pgvector and bi-temporal predicates are inherently slower than Moon's native HNSW plus TEMPORAL.SNAPSHOT_AT. Ingest is allowed up to 50ms p50 on the no-graph recipe; with graph extraction enabled, 300ms p50 because Gemma-3 4B local inference adds significant overhead.

### Latency Budget Allocations

Per blueprint section 4.1, the ingest budget breaks down as: chunker 2ms, embedder batched 8ms, serialization plus assemble ops 4ms, atomic_write to Moon 30ms, total p50 around 44ms. Per blueprint section 4.2, the recall budget breaks down as: query embed 3ms, vector_search 10ms, BM25 5ms, RRF fold client-side or Moon-native 2ms, rerank 12ms, hit hydration 3ms, total p50 around 35ms when reranker is enabled.

### Compressed-Timeline Risks

EmbeddingGemma local inference and the bge-reranker integration are the latency-budget gambles for v0. Mitigations are documented in CONTEXT.md and include swapping to the Ollama backend if local inference busts the per-call budget, deferring rerank to the next phase if cold-start model loading exceeds the budget, and shipping Phase 2 on Moon only if Postgres p50 misses by more than 2x.

## Conclusion

Lunaris is a focused memory engine. Six primitives, eight `StoragePort` methods, one retrieval DSL, two backends co-equal day zero. Everything that is not in scope for v0 is tracked under the v1 parallel track, with a clear deferral reason and target phase. The seven-day sprint to alpha tag is risk-managed by automated quality gates on every push.
