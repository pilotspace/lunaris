# How Recall Works — Memory Structure & the Millisecond Budget

This page answers two questions evaluators keep asking:

1. **How is a memory actually structured** once Lunaris has ingested it?
2. **Where do the milliseconds go** on a recall — i.e. *how* does the
   sub-25 ms contract hold, mechanically?

It mirrors the canonical
[`docs/ARCHITECTURE.md`](https://github.com/pilotspace/lunaris/blob/main/docs/ARCHITECTURE.md)
sections of the same names. Every claim is anchored to a code path or a
published benchmark.

## How a memory is structured

One ingested episode fans out into a small constellation of rows, all
minted under the canonical keyspace `lunaris:{scope}:{kind}:{ulid}`
(`lunaris_core::keyspace`) and all committed by the **same
`atomic_write`** (invariant INGEST-04 — see
[Core Concepts](../getting-started/concepts.md)):

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

Three structural decisions carry the whole recall story:

- **The chunk is the retrieval unit; the episode is the provenance
  unit.** Vector and BM25 hits return chunk ULIDs. Hydration walks
  `chunk → episode_id → episode`, so every hit arrives with its source
  attached.
- **One document, two indices' worth of duty.** On Moon, a chunk's
  embedding, BM25-tokenized text, `TAG` filter fields, and bi-temporal
  stamp live in a **single `HSET` document** indexed by one per-scope
  `FT` index (`lunaris_{scope}_{kind}_idx`). There is no "sync the
  vector DB with the text index" job because there is nothing to sync.
- **Every row is bi-temporal.** The `bt` field records *system time*
  (when Lunaris learned the fact) and *valid time* (when it was true in
  the world) as two half-open intervals. `forget` and supersession
  close intervals instead of destroying rows — which is why
  `.as_of(ts)` can answer "what did the agent believe last Tuesday?"

## Where the milliseconds go — anatomy of a recall

The sub-25 ms contract is not one trick — it is the **absence of four
round trips**. Here is how a typical
`vector.and(keyword).fuse_rrf(60)` recall spends its budget:

| Stage | What happens | Why it's fast |
|---|---|---|
| 1. Query embed | granite-embedding-311m runs **in-process** on llama.cpp (CPU, Q4_K_M GGUF) | No HTTP hop to an embedding server — the single biggest win (see the 86 ms lesson) |
| 2. Hybrid search | ONE `FT.SEARCH` HYBRID round trip; Moon fuses vector KNN + BM25 with **native RRF** server-side | Fusion happens inside the engine that owns both indices — not N queries glued together in app code |
| 2a. Filters & time | `TAG` pre-filters (`@source:{...}`) and `AS_OF <ms>` resolve **inside** the same search command | Filtering before scoring; the temporal cut never becomes an app-side post-filter |
| 3. Hydrate | Every hit's chunk row fetched **concurrently** (ordered fan-out, one `HMGET` per row); parent episodes fan out once per unique `episode_id` | Concurrent requests pipeline over one multiplexed connection — k hydrations cost ~1 batch of round trips, not 2k serial ones. Since-deleted chunks are skipped, not errored |
| 4. Rerank (opt-in) | bge-reranker-v2-m3 cross-encoder, in-process | **A quality stage, not a latency-class stage** — measured **p50 1301.3 ms** at the default `top_in=60` (575.6 ms at `top_in=30`), ~100× the blueprint's 12 ms allocation. Off by default; enabling it voids the 25 ms p50 contract ([`capacity.md` §4](https://github.com/pilotspace/lunaris/blob/main/docs/operations/capacity.md)) |

### CJK and other case-less scripts — vector-only auto-planning

Two v0 behaviours to know if your corpus or queries are in Chinese,
Japanese, or Korean (or any script without case):

- **The auto-planner never picks the keyword leg for CJK queries.** The
  `plan_query` helper (exported by `lunaris-retrieve`; RETRIEVE-13)
  chooses `Hybrid` (vector + BM25) only when it sees an entity-like
  **ASCII-uppercase** token mid-query — an English-only heuristic
  (`crates/lunaris-retrieve/src/planner.rs`). CJK text has no ASCII
  uppercase, so a CJK query **always plans `VectorOnly`** and BM25 is
  never consulted on that path. This is pinned by the
  `cjk_query_always_plans_vector_only` unit test in `planner.rs`, so the
  behaviour change will be visible when the graph-anchored planner
  replaces the heuristic. The multilingual granite-r2 embedder carries
  CJK recall in the meantime — and note this only affects *auto-planned*
  recall: an **explicit** DSL query (`vector.and(keyword).fuse_rrf(60)`)
  runs exactly the legs you wrote, in any script.
- **Sentence segmentation splits on ASCII terminals only.** The ingest
  chunker's sentence mode splits paragraphs on `.` `!` `?`
  (`crates/lunaris-ingest/src/chunker/segment.rs`); the full-width
  `。` `！` `？` terminals are not split points, so CJK prose in
  sentence mode degrades to paragraph-sized units. Chunks stay
  retrievable (the embedder is multilingual), just coarser.

### The 86 ms lesson (historical — v0.1.1, 2026-04-23)

The same 10k-document SQuAD harness (`scripts/bench-squad-kb.py`)
measured:

- **p50 86 ms** when query embedding went through an out-of-process
  Ollama HTTP server, and
- **~11 ms** on the strict-replay path that removes that hop
  (`scripts/ollama-replay-server.py` + `scripts/precompute-embeds.py`).

> **Treat the absolute numbers here as retired.** That run used Ollama +
> EmbeddingGemma 300M at k=3 on a 10k corpus — a stack removed in v0.4
> (Ollama) and again in v0.6 (candle). What survives is the *ratio*, not
> the milliseconds. The current latency envelope is the GA-2b one in
> [`capacity.md`](https://github.com/pilotspace/lunaris/blob/main/docs/operations/capacity.md).

The engine's own search + hydrate path was ~10 ms all along; the
network hop to the embedder was ~75 ms of pure overhead. That
measurement is why v0.4 moved embedding **in-process as the default**
— the shipped configuration is the configuration the contract was
proven on. (v0.4 ran the in-process embedder on candle; the v0.6
llama.cpp-only cutover (`docs/decisions/2026-07-10-llamacpp-only-cutover.md`)
replaced candle with llama.cpp as the in-process runtime — the
"no network hop" property this lesson describes is unchanged.)

The same decomposition repeated on Moon v0.3.0 with the 4-bit GGUF
granite embedder (3k-doc SQuAD train corpus): end-to-end p50 61.5 ms,
retrieval-only p50 3.1 ms / p99 3.6 ms — the gap is now in-process
quantized embedding compute, not a network hop, and the engine path
still sits far inside the 25 ms contract
([v0.3.0 rerun](https://github.com/pilotspace/lunaris/blob/main/docs/benchmarks/v0.7-moon-v030-rerun.md)).

### The 97 ms tail lesson

Hydration used to await one storage read per hit, serially — at k=30
that chain of round trips amplified every scheduler hiccup into the
tail. The 2026-06-10 fan-out change (one `HMGET` per row, all rows
concurrently over the multiplexed connection) flattened a measured
**p50 12 ms / p99 97.3 ms** at k=30 into **p50 6.0 ms / p99 6.2 ms** —
the tail now sits inside the p50 contract. Methodology and the full
A/B table: `docs/benchmarks/v0.6-recall-fanout-ab.md`.

### Why a fan-out stack can't follow

A typical agent-memory deployment runs a vector DB + a text-search
engine + a graph DB + a message broker. That stack pays:

1. one network round trip *per lane*, plus app-side fusion;
2. no shared `TAG` / temporal pushdown — you over-fetch, then
   post-filter in application code;
3. no common snapshot — the lanes can disagree about what exists.

Lunaris-on-Moon spends its entire budget inside one process and one
index, with one transaction boundary across all four lanes.

## Where to go next

- The query language itself: [The Retrieval DSL](./retrieval-dsl.md)
- The primitives and the keyspace:
  [Core Concepts](../getting-started/concepts.md)
- Backend setup and its honest limits:
  [The Storage Backend](../operations/backends.md)
- The published numbers:
  [`docs/benchmarks/v0.2.x/`](https://github.com/pilotspace/lunaris/tree/main/docs/benchmarks/v0.2.x)
