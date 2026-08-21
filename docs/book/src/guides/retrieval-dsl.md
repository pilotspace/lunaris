# The Retrieval DSL

> **DSL** = *Domain-Specific Language* — here, a small composable **query API**
> (operators you chain into a *plan*), not a separate language you write or
> parse. Prefer `ScopedLunaris::recall(query)` for a one-shot default query;
> reach for the DSL builder below when you need to compose operators.

**Reach for this chapter for every read beyond a single-key fetch.** The DSL
composes a small set of operators — `Vector`, `Keyword`, `Graph`, `Tree`
(RAPTOR hierarchical), plus fusion / rerank / modifier wrappers — into a
*plan*, then executes it in one pass and returns `Vec<Hit>`. It is declarative ("feel like Keras, not like a
query language", blueprint §8) and `tower::Service`-shaped at the edge so
rate-limit / retry / timeout / tracing middleware drops in for free.

All names below are re-exported at the `lunaris::` top level — never reach
into `lunaris_retrieve::`.

## Two ways to query

There are **two query forms over the same engine**. Both run on the same
storage / embedder / scope; the only difference is how much of the plan you
spell out.

**Form A — `scoped.recall(query)` (one-shot).** A single `.await` that returns
`Vec<Hit>` directly. It runs the **default plan**: a `Vector` search over the
`chunks` index — **no keyword fusion, no graph, no rerank**. Reach for it when a
plain semantic lookup is all you need.

```rust
use lunaris::{Lunaris, Query, Scope};

let lunaris = Lunaris::open("moon://localhost:6380").await?;
let scoped  = lunaris.scoped(Scope::new("acme.agent-1")?);

// One call, Vec<Hit> back. Default plan = Vector over `chunks`.
let hits = scoped.recall(Query::text("who loves chocolate")).await?;
```

**Form B — `scoped.dsl()…execute(query)` (composable).** Returns a
`RetrievalBuilder` you customise (`.with_root(...)`, `.filter(...)`, `.as_of(...)`,
`.rerank(...)`) before the single `.execute(query).await`. Reach for it the
moment you want hybrid fusion, the graph or tree operators, time-travel, or
reranking.

```rust
use lunaris::{Keyword, Query, Vector};

// `scoped` is the handle from Form A above.
let hits = scoped
    .dsl()
    .with_root(Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(5))
    .execute(Query::text("who loves chocolate"))
    .await?;
```

They are **the same machinery**: `recall(query)` is exactly `dsl()` with the
default root left in place, then `.execute(query)` — verify it in
`crates/lunaris/src/handle.rs` (`recall` at the `ScopedLunaris` impl delegates to
`engine.recall().with_scope(scope).execute(query)`; `dsl()` returns the same
pre-seeded builder). So there is nothing `recall()` can do that `dsl()` cannot —
`recall()` is the convenience name for the common default.

| You want… | Use | Returns |
|---|---|---|
| A plain semantic lookup, least ceremony | `scoped.recall(query)` | `Vec<Hit>` |
| Hybrid (vector + BM25) fusion | `scoped.dsl().with_root(Vector….and(Keyword…).fuse_rrf(k))` | builder → `Vec<Hit>` |
| Graph expansion / RAPTOR `Tree` descent | `scoped.dsl().with_root(Graph… / Tree…)` | builder → `Vec<Hit>` |
| Time-travel (`as_of`), filters, rerank | `scoped.dsl().as_of(…)/.filter(…)/.rerank(…)` | builder → `Vec<Hit>` |

**See it run.** [Querying Three Ways](../cookbook/querying-three-ways.md) runs
all three forms — direct recall, DSL fusion, and the `Tree` operator — over one
ingested document.

> Mind the three `recall` names. **`ScopedLunaris::recall(query)`** (above)
> returns `Vec<Hit>` and is the canonical one-shot. **`ScopedLunaris::dsl()`**
> returns the builder. The bare **`Lunaris::recall()`** (no scope) is a
> *legacy* path that returns a `RetrievalBuilder` (not `Vec<Hit>`) seeded with
> `Scope::dev()` and warns on every call — see the note under [Seeding a
> builder](#seeding-a-builder).

## Seeding a builder

`ScopedLunaris::dsl()` returns a `RetrievalBuilder` pre-seeded with the
handle's storage / embedder / keyword Arcs **and the bound scope**, so only
hits from that scope's partition come back. Its default root operator is
`Vector::new("chunks", 30)`; you replace it with `.with_root(...)`.

```rust
use lunaris::{Keyword, Lunaris, Query, Scope, Vector};

let lunaris = Lunaris::open("moon://localhost:6380").await?;
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
fills `chunks`; the extractor fills `entities` and `facts`. RAPTOR's
ingest-time tree now fills `communities` with embedded **summary** nodes — query
them directly here, or via the **`Tree`** operator (below) for hierarchical
descent. (The [consolidator](./consolidate-verify.md)'s Leiden run also
contributes community nodes.) (`crates/lunaris-retrieve/src/operators/vector.rs`)

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

### `Tree::new(index, k)` — RAPTOR hierarchical retrieval

Climbs the RAPTOR community tree instead of searching flat chunks. It
vector-searches the `communities` index for the `k` nearest **summary** nodes,
then descends `Community.members` breadth-first to collect the leaf chunks
beneath them. Because a summary node *semantically aggregates* its chunks, this
surfaces whole-document and multi-hop answers whose constituent chunks fall
outside a flat search's top-`k` budget.

```rust
use lunaris::{Query, Tree};

// operator form — pass to .with_root() or compose with .and()/.fuse_rrf()
scoped.dsl()
    .with_root(Tree::new("communities", 5))
    .execute(Query::text("What are the main themes across both reports?"))
    .await?;

// builder shortcut — .tree(index, k, depth) replaces the root in one call
scoped.dsl()
    .tree("communities", 5, 1)
    .execute(Query::text("What are the main themes across both reports?"))
    .await?;
```

- **`k`** — number of top community summary nodes to seed from (clamped to `MAX_K`).
- **`depth`** — BFS descent levels. `1` (default, `DEFAULT_TREE_DEPTH`) collects
  the seed communities' direct members; `2` also expands sub-communities one
  level deeper, and so on. Clamped to `[1, MAX_TREE_DEPTH]` where
  `MAX_TREE_DEPTH = 4`. **Only the first level issues a vector search** — deeper
  levels read community KV rows only, so cost scales with depth × fan-out, not
  index size.
- Composes like any operator: `.and()` / `.or()` / `.then()` / `.fuse_rrf()` /
  `.top()`. Fuse it with a flat `Vector` branch to get pinpoint chunks **and**
  tree-aggregated coverage in one plan:

```rust
use lunaris::{Query, Tree, Vector};
scoped.dsl().with_root(
    Vector::new("chunks", 30)
        .and(Tree::new("communities", 5))
        .fuse_rrf(60)
        .top(10),
).execute(Query::text("Summarize the incident and its root cause")).await?;
```

> **Prerequisite:** the `communities` index must be populated. RAPTOR fills it at
> ingest (community `summary_embedding`, since the 2026-06-04 change). If `.tree()`
> comes back empty, the scope hasn't ingested a document large enough to build a
> tree yet — see [Ingesting Observations](./ingest.md).

> **What's proven today:** `.tree()` is verified *wired and traversed* — on a
> whole-document query it returns the full leaf set where flat top-`k` returns a
> single chunk. Relevance-vs-flat ranking with a production embedder is **not yet
> benchmarked**; treat `.tree()` as a coverage / recall lever, fused with
> `Vector` for precision. (`crates/lunaris-retrieve/src/operators/tree.rs`)

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
`StorageCapabilities::native_rrf`). Anything else folds client-side
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

The default reranker is BGE-Reranker-v2-m3. **Budget seconds, not
milliseconds:** it measures **p50 1301.3 ms** at the default `top_in=60`
(575.6 ms at `top_in=30`), plus a one-time ~1.0–1.4 s lazy GGUF load on the
first reranked recall of the process ([`capacity.md` §4](https://github.com/pilotspace/lunaris/blob/main/docs/operations/capacity.md)). Rerank is a
quality stage; enabling it voids the 25 ms p50 recall contract and the 100 ms
latency SLO. When its
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
  index that nothing wrote to. (`communities` is now populated at ingest by
  RAPTOR — if it's empty, this scope hasn't ingested a document big enough to
  build a tree.) Drop the filter and re-run.
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
