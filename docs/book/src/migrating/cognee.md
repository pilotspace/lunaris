# Migrating from Cognee

> Adapted from `docs/MIGRATING-FROM-COGNEE.md` (kept in the repo as the
> standalone version).

Cognee and Lunaris both treat the knowledge graph as a first-class
substrate. The difference is the surface: Cognee is a Python
pipeline ("Tasks → DataPoints → Pipelines") that produces a
queryable graph; Lunaris is an embedded Rust core with a composable
retrieval DSL that lets you query a bi-temporal graph + vector +
keyword store in a single fused call.

This page maps Cognee concepts to their Lunaris equivalents so a team
already running Cognee can evaluate the switch with concrete code.

> **TL;DR** — if your agent depends on Cognee's pipeline plug-in
> ecosystem (custom extractors, custom chunkers, custom graph
> builders), Cognee is well-positioned; the pipeline composition is
> its strength.
> If you want sub-25 ms recall over a bi-temporal store + a
> retrieval DSL where vector + graph + keyword fuse in one
> typed query, Lunaris's composable operator surface is the
> simpler model.

## At a glance

| Concern                                    | Cognee                                          | Lunaris                                                |
|---------------------------------------------|-------------------------------------------------|--------------------------------------------------------|
| **Runtime**                                | Python                                          | Embedded Rust core + Python (PyO3) + TypeScript (NAPI) bindings |
| **Storage**                                | Vector DB (LanceDB / Qdrant / Weaviate / ...)  + Graph DB (Neo4j / FalkorDB / Memgraph / ...) — pluggable | Moon (one substrate, FT.* + graph + KV native) OR Postgres (pgvector + AGE + pgmq) |
| **Composition model**                      | Pipeline of Tasks operating on DataPoints       | Composable retrieval DSL (`vector`, `keyword`, `graph` + `.and / .or / .then / .fuse_rrf`) |
| **Bi-temporal**                            | Not first-class (DataPoints can carry timestamps but the engine doesn't model `(valid_time, sys_time)` tuples) | First-class `(valid_time, sys_time)` per row; `.as_of(ts)` is one combinator |
| **Recall latency**                         | Depends on backend (~50 ms LanceDB local, ~200 ms cloud) | p50 ≤ 25 ms / p99 ≤ 100 ms on `laptop-arm64`           |
| **Atomicity**                              | Per-store best-effort                            | One `atomic_write` covers vector + KV + graph + audit + queue. CI gate enforces single call site |
| **Tenancy**                                | `dataset` string on the API                     | `Scope` newtype (`[A-Za-z0-9_\-.]{1,128}`) threaded through every storage call + Postgres RLS-enforced |
| **Graph query language**                   | Cypher (via backend)                            | AGE Cypher (Postgres) OR Moon native graph; same `Graph::anchored(entity_ids, hops)` operator on both |
| **Custom pipeline tasks**                  | First-class — register Tasks, compose with `await cognee.cognify()` | Override `Extractor` trait (Phase 3); recall DSL is fixed surface |
| **License**                                | Apache 2.0                                      | Apache 2.0                                             |

## Where Cognee and Lunaris differ in spirit

Cognee is **pipeline-oriented**: your agent's ingest path is a
sequence of Tasks (chunk, extract, embed, link, store), and the
*power* is that you can compose, replace, or insert Tasks as your
domain evolves. The cost is that the surface is wide and a typo in
one Task can break the entire `cognify()` call.

Lunaris is **operator-oriented**: the ingest path is fixed
(`Lunaris::ingest()` chunks + embeds + writes in one
`atomic_write`); the *power* is in the retrieval DSL where you
compose `vector`, `keyword`, `graph`, fusion, and time-travel into a
typed query tree. The cost is that custom ingest logic means a
custom `Extractor` impl rather than a Task plug-in.

Both are right for different shapes. Cognee is the answer when your
domain logic lives at ingest time (custom DataPoint relationships,
domain-specific Tasks). Lunaris is the answer when your domain logic
lives at recall time (hybrid search with custom fusion, bi-temporal
audit queries, graph-anchored exploration).

## Code-side comparison

### Ingest

**Cognee**

```python
import cognee

await cognee.add("Alice joined Acme on 2024-04-01.", dataset_name="bio")
await cognee.cognify(["bio"])  # runs the default Tasks pipeline:
                               # chunk → extract → embed → link → store
```

**Lunaris**

```python
import lunaris

mem = lunaris.Lunaris.open("moon://localhost:6380")
scope = lunaris.Scope("bio")
mem.scoped(scope).ingest(
    lunaris.EpisodeBuilder("chat:session-1/turn-1",
                            "Alice joined Acme on 2024-04-01.")
)
# Chunking + embedding + atomic write happen inside `ingest()`.
# Graph extraction is opt-in via the graph pipeline toggle.
```

### Recall — semantic

**Cognee**

```python
results = await cognee.search(
    query_text="when did Alice join Acme?",
    search_type=cognee.SearchType.GRAPH_COMPLETION,
)
```

**Lunaris**

```python
hits = await (
    mem.scoped(scope)
       .recall()                              # pre-bound builder, default root Vector("chunks", 30)
       .top(5)
       .execute()                             # plan collapses to one FFI call; no query-text arg yet
)
```

### Recall — hybrid semantic + graph

**Cognee**

```python
results = await cognee.search(
    query_text="who does Alice work with at Acme?",
    search_type=cognee.SearchType.GRAPH_COMPLETION,
)
# search_type=GRAPH_COMPLETION runs an LLM over the graph context;
# you can't compose "vector AND graph anchored on Alice" without
# dropping to the underlying backends.
```

**Lunaris**

```python
hits = await (
    mem.scoped(scope)
       .recall()                  # default root Vector("chunks", 30)
       .and_(lunaris.Graph.anchored(entity_ids=[alice_id], hops=2))
       .fuse_rrf(60)
       .top(5)
       .execute()
)
```

The `.and_` fans out vector + graph branches concurrently; `.fuse_rrf`
folds them with Reciprocal Rank Fusion. The whole pipeline is one
typed expression; no LLM round-trip for the fusion step. See
[The Retrieval DSL](../guides/retrieval-dsl.md) and
[The Graph Pipeline](../guides/graph.md).

### Custom extraction

This is where Cognee's pipeline model shines. If you need a
domain-specific entity extractor, in Cognee you write a Task; in
Lunaris you implement the `Extractor` trait.

**Cognee**

```python
from cognee.tasks.documents import classify_documents
from cognee.modules.pipelines import Pipeline

async def my_task(data_points):
    # ... domain-specific logic
    return data_points

pipeline = Pipeline(tasks=[classify_documents, my_task])
await pipeline.run(dataset_name="bio")
```

**Lunaris**

```python
class MyExtractor(lunaris.Extractor):
    async def extract(self, content: str) -> lunaris.ExtractionResult:
        # ... domain-specific logic
        return lunaris.ExtractionResult(entities=[...], relations=[...])

mem = lunaris.Lunaris.open("moon://localhost:6380").with_extractor(MyExtractor())
```

If you have N custom Tasks composing into a pipeline, Cognee's
model maps cleaner — composing N Lunaris extractors requires
wrapping them in a single trait impl that fans out internally.
v0.3 [RFC 0007](../appendix/index.md) (`FallbackExtractor` /
`FallbackEmbedder` combinators) adds the fan-out primitive.

### Time-travel recall

> **Backend note (v0.6.2).** `.as_of(<past timestamp>)` needs a backend that
> keeps a KV version chain to hydrate the historical rows: **Postgres and
> SQLite** answer these. On **Moon** the call returns
> `StorageError::NotSupported` (HTTP `501 not_supported`) — Moon stores
> Lunaris rows as plain hashes, and since v0.6.2 it refuses a historical pin
> rather than silently answering with present-time data. Moon's search and
> graph lanes stay temporal (`FT.SEARCH AS_OF`, `GRAPH.QUERY VALID_AT`).

Cognee doesn't model bi-temporal queries first-class. The closest is
filtering DataPoints by a `created_at` field post-search.

**Lunaris**

```python
snapshot_ms = int(snapshot_ts.timestamp() * 1000)
hits = await (
    mem.scoped(scope)
       .recall()                  # default root Vector("chunks", 30)
       .as_of(snapshot_ms)        # ms since the Unix epoch
       .execute()
)
```

The temporal cut happens at the storage layer (`tstzrange &&` on
Postgres, native bi-temporal on Moon). On 1M-fact corpora the
latency difference matters.

## Migration checklist

1. **Stand up Lunaris alongside Cognee.** Use `examples/quickstart-py`.
2. **Map `dataset` → `Scope`.** Same `[A-Za-z0-9_\-.]{1,128}`
   alphabet constraint as the [Zep migration](./zep.md).
3. **Port custom Cognee Tasks to a Lunaris `Extractor` impl.**
   This is the largest migration cost — a Cognee deployment with
   5 custom Tasks becomes ~150 lines of trait impl. If you have
   no custom Tasks, this step is zero work.
4. **Mirror writes.** Every `cognee.add(...)` + `cognee.cognify(...)`
   is also dispatched to `mem.scoped(scope).ingest(...)`.
5. **Shadow reads.** Every `cognee.search(...)` is also issued to
   Lunaris's recall DSL. Diff in your eval harness.
6. **Cutover and decommission.** You now run one Rust process +
   Moon/Postgres instead of Cognee + (vector DB) + (graph DB).

## When to stay on Cognee

- Your custom Tasks are non-trivial and porting them to a single
  `Extractor` impl is too much migration cost.
- You're using Cognee's pipeline plug-in ecosystem (community
  Tasks) and that's load-bearing for your stack.
- You're committed to a specific vector DB / graph DB combination
  that Lunaris doesn't ship a backend for, and you don't want to
  operate Moon or Postgres.
- Pure Python deploy, no Rust binary in the build pipeline.

## Known gaps vs Cognee today

- **No Task plug-in ecosystem.** Lunaris ships a fixed ingest path
  (`Lunaris::ingest`). Custom logic goes in the `Extractor` trait
  impl — one impl, not a chain. v0.3 RFC 0007 adds composable
  fallback combinators for resilience but does not introduce a
  pipeline DSL.
- **Backend matrix is smaller.** Lunaris ships Moon + Postgres; no
  LanceDB / Qdrant / Weaviate adapter today. The `StoragePort`
  trait is the extension point — a third-party crate can implement
  the trait for any backend.
- **Graph-completion search.** Cognee's `GRAPH_COMPLETION` search
  type wraps an LLM call over the graph context. Lunaris exposes
  the graph traversal as an operator (`Graph::anchored`) and leaves
  the LLM call to the caller. If you want one-call
  "graph-and-summarize", you'd compose `recall() + extractor.summarize()`
  yourself.

See the [Mem0](./mem0.md) and [Zep](./zep.md) pages for the parallel
migration stories from the other two incumbents. The trio covers the
three distinct positioning conversations: Mem0 (no bi-temporal
upgrade required), Zep (latency + substrate simplification), Cognee
(pipeline-vs-DSL tradeoff).
