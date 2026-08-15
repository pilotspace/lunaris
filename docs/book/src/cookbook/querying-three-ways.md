# Querying Three Ways

**Reach for this page when you want to see the raw retrieval surface — not a
recipe wrapper.** One ingested document, one question, asked three ways: the
one-shot `recall(query)`, a composed DSL plan that **fuses** flat chunks with
RAPTOR tree descent, and the `Tree` operator on its own.

> **This page used to be the zero-deps SQLite tour.** 0.7.0 deleted the
> embedded backend, so it now needs a Moon like every other page:
>
> ```bash
> docker run -d -p 6380:6379 ghcr.io/pilotspace/moon:0.8.5 \
>   --shards 1 --protected-mode no --appendonly yes
> ```
>
> What it still is: the *unwrapped* surface. The recipe pages
> (`DocumentKnowledgeBase`, …) hand you a prebuilt
> `Vector + Keyword(BM25) ⊕ RRF` plan; this page composes the operators by
> hand so you can see what a plan is made of.

## The three forms

| Form | Call | Use it when… |
|---|---|---|
| One-shot | `scoped.recall(query)` | a plain semantic lookup is enough |
| DSL fusion | `scoped.dsl().with_root(Vector….and(Tree…).fuse_rrf(k))….execute()` | you want to blend flat chunks with hierarchical context |
| Tree | `scoped.dsl().with_root(Tree::new("communities", k).with_depth(2))…` | a whole-document question whose answer spans many chunks |

All three are the **same engine**: `recall(query)` is just `dsl()` with the
default root left in place. See [Two ways to
query](../guides/retrieval-dsl.md#two-ways-to-query) for the distinction.

## Example

Shaped after the ingest fixtures in
`crates/lunaris-ingest/tests/raptor_wiring.rs` (which ingest a headed document
and assert RAPTOR builds communities with 768-d summary embeddings) and the
`Tree` discrimination benchmark in
`crates/lunaris-retrieve/tests/tree_recall.rs`.

```rust,no_run
use anyhow::{Context, Result};
use lunaris::{EpisodeBuilder, Lunaris, Query, Scope, Tree, Vector};

/// A multi-section document, sized like `MULTI_SECTION_DOC` in the
/// `tree_recall.rs` test we cite. RAPTOR builds an H1 → H2 → chunk community
/// tree at ingest, so `communities` is populated and `Tree` at `depth=2` has
/// sub-communities to descend into. The chunker targets ~500 tokens, so the doc
/// must clear that threshold (~two padded headed sections) to produce ≥2 chunks
/// and a non-flat topology — a one-paragraph blurb collapses to a single chunk
/// and `Tree` would return nothing.
const DOC: &str = "# Agent Memory Architecture

## Section A: Core Design

The agent memory system uses a bi-temporal MVCC store. Each observation is
recorded with both a valid-time and a transaction-time. This dual timestamp
enables point-in-time queries and auditing of historical states: we can ask
what was known at any point in transaction time, and what was true at any
point in valid time. That is essential for agents reconciling information
gathered at different moments and reasoning about how the world has changed.

Writes commit through a single atomic_write, so a multi-primitive ingest —
episode row, chunk rows, vector upserts, and the RAPTOR community tree — is
all-or-nothing. There is no window in which the chunks exist but their
community summaries do not; a reader either sees the whole ingest or none of
it. This is the atomicity contract that fan-out architectures cannot make.

Memory isolation between agents uses scope partitioning. Each agent receives a
unique scope key that prefixes its KV entries and FT index slots, so the
backend enforces isolation at the data layer rather than trusting application
code to filter correctly. A misconfigured caller cannot read another agent's
memories, because the partition boundary is encoded into the keyspace itself.

## Section B: Retrieval and Performance

Recall fuses semantic vector search with BM25 keyword search using Reciprocal
Rank Fusion, which combines per-branch reciprocal ranks into one score that is
robust to the scale differences between cosine similarity and normalized BM25.
Each branch contributes independently, and the fused score reflects consensus
across retrieval strategies rather than the idiosyncrasies of either one.

RAPTOR organises related chunks into a hierarchy of summary communities. Each
community node aggregates the semantic content of its child chunks into a
single embedded vector, so a whole-document question can match the summary
node and then descend to every leaf chunk underneath it — including chunks
that would never score into a flat top-k on their own. This is the core
insight: summarise at multiple granularities, then match at the right level.

The system targets sub-25ms recall over millions of bi-temporal facts. Vector
search runs in single-digit milliseconds, community summary embeddings let
whole-document queries bypass flat chunk retrieval, and no LLM sits on the read
path. Summaries are embedded with the same model as chunks, so cosine scores
are directly comparable across the chunks and communities indices.
";

#[tokio::main]
async fn main() -> Result<()> {
    // `moon://host:port` is the only scheme 0.7.0 accepts.
    let lunaris = Lunaris::open("moon://127.0.0.1:6380").await.context("open")?;
    let scoped = lunaris.scoped(Scope::new("demo").context("scope")?);

    // Ingest once. The umbrella pipeline chunks + embeds + builds the RAPTOR
    // community tree, all under one atomic_write (INGEST-04).
    scoped
        .ingest(EpisodeBuilder::new("demo:architecture.md", DOC))
        .await
        .context("ingest")?;

    let question = "What are the main themes across the whole document?";

    // ── Form 1: one-shot recall ────────────────────────────────────────────
    // Default plan = Vector over `chunks`. No fusion, no rerank. Vec<Hit> back.
    let flat = scoped.recall(Query::text(question)).await.context("recall")?;
    println!("[recall]  {} hit(s)", flat.len());

    // ── Form 2: DSL fusion (flat chunks ⊕ RAPTOR tree) ─────────────────────
    // Compose two vector-backed operators and fold their rankings with RRF.
    // Both branches run on the embedded backend — no server needed.
    let fused = scoped
        .dsl()
        .with_root(
            Vector::new("chunks", 20)
                .and(Tree::new("communities", 3).with_depth(2))
                .fuse_rrf(60)
                .top(8),
        )
        .execute(Query::text(question))
        .await
        .context("recall (dsl fusion)")?;
    println!("[fusion]  {} hit(s)", fused.len());

    // ── Form 3: Tree on its own (RAPTOR hierarchical descent) ──────────────
    // Find the nearest community summary, then descend to its leaf chunks.
    // depth=2 walks H1 root → H2 sub-communities → leaf chunks.
    let tree = scoped
        .dsl()
        .with_root(Tree::new("communities", 1).with_depth(2))
        .execute(Query::text(question))
        .await
        .context("recall (tree)")?;
    println!("[tree]    {} hit(s)", tree.len());

    Ok(())
}
```

### What's proven where

- **Ingest builds communities.** `raptor_wiring.rs` ingests a headed document
  and asserts each community carries a 768-d `summary_embedding` — the
  `communities` vector index is populated at ingest, via the single
  `atomic_write` in `crates/lunaris-ingest/src/pipeline.rs`.
- **`Tree` uses only `vector_search` + `read_as_of`** (see
  `crates/lunaris-retrieve/src/operators/tree.rs`) — both core `StoragePort`
  methods, no Cypher and no `GRAPH.QUERY`.
- **The *tree-beats-flat* discrimination benchmark** (`tree_recall.rs`) is
  measured against Moon, which since 0.7.0 is the only place it could be
  measured.

## Scaling up: hybrid BM25 fusion

The classic hybrid plan fuses **semantic** vector search with **lexical** BM25:

```rust,ignore
use lunaris::{Keyword, Query, Vector};

let hits = scoped
    .dsl()
    .with_root(
        Vector::new("chunks", 30)
            .and(Keyword::bm25("chunks", 30))
            .fuse_rrf(60)
            .top(5),
    )
    .execute(Query::text("who loves chocolate"))
    .await?;
```

The `Keyword` branch rides Moon's native inverted index, and when both legs sit
on the same index `fuse_rrf` collapses them into one round trip instead of two.
For a batteries-included hybrid-RAG wrapper over the same plan, reach for
[`DocumentKnowledgeBase`](./document-kb.md).

## Notes

- **`recall(query)` ≠ hybrid.** The one-shot form is pure vector over `chunks`.
  If you reached for `recall()` expecting fusion or rerank, use `dsl()` instead.
- **`Tree` returns empty when the scope has no communities.** A freshly-created
  scope (nothing ingested) or a single-paragraph document that collapses to a
  flat topology will yield zero tree hits — that is the normal graceful-empty
  state, not an error. Ingest a multi-section document first.
- **Depth costs fan-out, not index scans.** `Tree::with_depth(d)` issues one
  vector search, then walks community KV rows up to `d` levels — latency scales
  with depth × fan-out, capped at `MAX_TREE_DEPTH = 4`.
- See [The Retrieval DSL](../guides/retrieval-dsl.md) for the full operator and
  combinator surface, and [The Storage Backend](../operations/backends.md) for
  the Moon setup this page assumes.