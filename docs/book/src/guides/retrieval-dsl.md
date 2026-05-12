# The Retrieval DSL

**Reach for this chapter for every read beyond a single-key fetch.** The DSL
composes a small set of operators — `Vector`, `Keyword`, `Graph`, plus
fusion / rerank / modifier wrappers — into a *plan*, then executes it in one
pass and returns `Vec<Hit>`. It is declarative ("feel like Keras, not like a
query language", blueprint §8) and `tower::Service`-shaped at the edge so
rate-limit / retry / timeout / tracing middleware drops in for free.

All names below are re-exported at the `lunaris::` top level — never reach
into `lunaris_retrieve::`.

## Seeding a builder

`ScopedLunaris::dsl()` returns a `RetrievalBuilder` pre-seeded with the
handle's storage / embedder / keyword Arcs **and the bound scope**, so only
hits from that scope's partition come back. Its default root operator is
`Vector::new("chunks", 30)`; you replace it with `.with_root(...)`.

```rust
use lunaris::{Keyword, Lunaris, Query, Scope, Vector};

let lunaris = Lunaris::open("moon://localhost:6379").await?;
let scoped  = lunaris.scoped(Scope::new("acme.agent-1")?);

let hits = scoped
    .dsl()
    .with_root(Vector::new("chunks", 30).top(5))
    .execute(Query::text("brown fox"))
    .await?;
```

> **`Lunaris::recall()` exists too**, but it seeds `Scope::dev()` and emits a
> `tracing::warn!` on every call (`crates/lunaris/src/recall.rs:78-80`). It is
> the v0.1 backwards-compatible path; new code uses `engine.scoped(scope).dsl()`
> (or `engine.scoped(scope).recall(query)` for the one-shot form).

`RetrievalBuilder` is synchronous — `with_root`, `filter` / `filter_str`,
`as_of`, `rerank`, `degraded_fallback` all run before any IO. `.execute(query)`
is the only `.await`; it wires the operator tree into a `QueryContext`, runs
it, and hydrates the results. This keeps the builder `Send` without
future-boxing.

## The operators

### `Vector::new(index, k)`

Top-`k` chunks by vector (cosine) similarity. `index` is one of the four
whitelisted names: `chunks | entities | facts | communities`. The chunker
fills `chunks`; the extractor fills `entities` and `facts`; nothing fills
`communities` until the [consolidator](./consolidate-verify.md)'s Leiden run
lands. (`crates/lunaris-retrieve/src/operators/vector.rs`)

### `Keyword::bm25(index, k)`

Top-`k` chunks by BM25 keyword score (min-max normalized).
(`crates/lunaris-retrieve/src/operators/keyword.rs`)

### `Graph::anchored(entity_ids, hops)`

Breadth-first traversal out from known entities — "everything we know about
Alice". `entity_ids` are pre-resolved `EntityId`s:

```rust
use lunaris::{EntityId, Graph};
let alice = EntityId::from_name_and_type("Alice", "Person");   // deterministic content hash
let g = Graph::anchored(vec![alice], 2);
```

`hops` is clamped to `[1, MAX_GRAPH_HOPS = 5]`; `DEFAULT_GRAPH_HOPS = 2`.
Empty `entity_ids` short-circuits to an empty result without touching storage.
Per-hit score is `1.0 / (1 + bfs_rank)`. `.with_k(n)` caps the candidate set
(default `DEFAULT_GRAPH_K = 30`); `.with_graph(name)` overrides the graph key
(default `lunaris_graph`). Requires the graph pipeline and an extractor — see
[The Graph Pipeline](./graph.md). (`crates/lunaris-retrieve/src/operators/graph.rs`)

### Combinators — `.and()` / `.or()` / `.then()`

Each operator carries `.and(other)`, `.or(other)`, `.then(other)`
(`crates/lunaris-retrieve/src/operators/combinators.rs`):

- **`.and(other)`** — run both retrievers; both result sets flow into the next
  operator (the typical input to `.fuse_rrf`).
- **`.or(other)`** — fall back to `other` only if the left side yields nothing.
- **`.then(other)`** — feed the left side's hits as the input to `other`
  (re-ranking / refinement chains).

### Fusion — `.fuse_rrf(k)`

Reciprocal-rank fusion over the upstream branches. Each branch contributes
`1 / (k + rank_i)`; `k = 60` is the conventional constant.

```rust
Vector::new("chunks", 30)
    .and(Keyword::bm25("chunks", 30))
    .fuse_rrf(60)
    .top(5)
```

**Moon-native vs client-side.** When the handle was opened against a `moon://`
URL **and** the shape is `Vector + Keyword(BM25)` on the *same* index,
`fuse_rrf` dispatches to Moon's native `text().hybrid_search` — one round trip
instead of two (`crates/lunaris-retrieve/src/operators/fuse.rs`, governed by
`StorageCapabilities::native_rrf`). Postgres always uses client-side RRF
(`crates/lunaris-retrieve/src/operators/fuse.rs::client_side_rrf`). **Any `Graph`
branch in the tree forces client-side RRF** — the Moon one-trip path only
fires for the Vector+Keyword(BM25) case. The API is identical either way.

### Modifiers — `.top(k)`, `filter_str`, `.as_of(ts)`

- **`.top(k)`** — cap the final result set. Available on every operator and on
  the builder.
- **`RetrievalBuilder::filter_str(s)`** — parse a v0 string predicate into a
  `Filter`. Returns a `Result<Self, FilterParseError>` *at builder time* so
  invalid syntax surfaces before any IO. The v0 grammar parses two predicate
  forms only — `field = 'value'` (→ `Filter::Eq`) and `field LIKE 'prefix%'`
  (→ `Filter::StartsWith`; the `%` must be the last character, no embedded
  `%`). Anything else is a parse error
  (`crates/lunaris-retrieve/src/operators/modifiers.rs:80`). Never `.unwrap()`
  on user input — propagate with `?`. `.filter(Filter)` takes a pre-built
  filter (use it for `Filter::And` / `Filter::Or` composition).
- **`RetrievalBuilder::as_of(ts)`** — pin the bi-temporal snapshot to an
  `Hlc`. Builder-level filter / as_of override the per-`Query` fields only if
  the query didn't already set them.

### `rerank(reranker)`

Wrap the current root with a cross-encoder rerank pass over the top
`DEFAULT_RERANK_TOP_IN = 30` candidates. Pass `lunaris.reranker()`:

```rust
scoped.dsl()
    .with_root(
        Vector::new("chunks", 30)
            .and(Keyword::bm25("chunks", 30))
            .fuse_rrf(60)
            .top(30),
    )
    .rerank(lunaris.reranker())
    .top(5)
    .execute(Query::text("brown fox"))
    .await?;
```

The default reranker is BGE-Reranker-v2-m3 (~12 ms p50 budget). When its
weights are missing, `Lunaris::open` installs `NoopReranker` and
`lunaris.reranker()` still returns a working `Arc<dyn Reranker>` that passes
scores through unchanged — `Hit::rerank_applied` is `false` in that case.
Builders constructed via `Lunaris::with_parts` get `NoopReranker` by default;
opt into rerank yourself in tests/benches.

### `degraded_fallback(fallback)`

Wrap the current root so that on **any error** from the primary it switches to
`fallback` and tags every returned hit with `Hit::degraded = true`
(`crates/lunaris-retrieve/src/operators/degraded.rs`). Pair it with
`Lunaris::recall_with_degraded_check()`, which reads the verifier queue depth
once and pre-flags every hit when the depth crosses
`LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` (default 1000) — see
[Consolidation & Verification](./consolidate-verify.md).

## The build-up in four steps

```rust
// 1 — pure vector
scoped.dsl().with_root(Vector::new("chunks", 30).top(5)).execute(Query::text("brown fox")).await?;

// 2 — add BM25
scoped.dsl().with_root(
    Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).top(5),
).execute(Query::text("brown fox")).await?;

// 3 — fuse with reciprocal rank
scoped.dsl().with_root(
    Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(5),
).execute(Query::text("brown fox")).await?;

// 4 — rerank
scoped.dsl().with_root(
    Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(30),
).rerank(lunaris.reranker()).top(5).execute(Query::text("brown fox")).await?;
```

Graph-aware: swap a branch for `Graph::anchored`:

```rust
use lunaris::{EntityId, Graph, Query, Vector};
let alice = EntityId::from_name_and_type("Alice", "Person");
scoped.dsl().with_root(
    Vector::new("chunks", 30)
        .and(Graph::anchored(vec![alice], 2))
        .fuse_rrf(60)
        .top(30),
).rerank(lunaris.reranker()).top(5).execute(Query::text("Tell me about Alice")).await?;
```

## The `Hit` you get back

```rust
pub struct Hit {
    pub id: Vec<u8>,            // backend id (a ULID's 16 bytes for chunks)
    pub score: f32,
    pub text: String,           // chunk body — from the chunk's KV row
    pub source: String,         // episode source, "" if the episode row is gone
    pub heading_path: Vec<String>,
    pub valid_from: Hlc,
    pub valid_to: Option<Hlc>,  // None = still valid
    pub degraded: bool,         // came from a degraded_fallback path
    pub rerank_applied: bool,   // the real cross-encoder ran (not the noop)
    pub source_op: SourceOp,    // which operator produced it (RRF groups by this)
}
```

`Query::text(t)` builds a default query (`k = 30`, no filter, no `as_of`); the
struct-literal form lets you set every field explicitly.

## `tower::Service`

For middleware-stacked use, `RetrievalService` implements
`tower::Service<Query, Response = Vec<Hit>, Error = LunarisError>`
(`crates/lunaris-retrieve/src/service.rs`), so
`tower::ServiceBuilder::new().rate_limit(..).timeout(..).retry(..).service(retriever)`
works. Note `RetrievalService` itself has no scope context (it uses
`Scope::dev()`); for scope-isolated reads go through `ScopedLunaris::dsl()`.

## Gotchas

- **Empty hits are usually a filter problem.** Over-tight `filter_str`, or an
  index that nothing wrote to (`communities` until v1). Drop the filter and
  re-run.
- **`filter_str` parse errors are builder-time, not execute-time.** Catch them
  with `?` where you build the chain.
- **`execute_raw`** returns un-hydrated `RawHit`s — for bench harnesses that
  measure search-path latency over non-`chunks` indices. Production callers use
  `execute` so "every hit has chunk text" holds.

## See also

- [Ingesting Observations](./ingest.md) — how the `chunks` index gets filled.
- [The Graph Pipeline](./graph.md) — populating `entities` / `facts` for
  `Graph::anchored`.
- [Cookbook → Document Knowledge Base](../cookbook/document-kb.md) — RRF-fused
  RAG without hand-composing the DSL.
- [Configuration Reference](../reference/configuration.md) — reranker /
  embedder backends and the degraded-check threshold.
