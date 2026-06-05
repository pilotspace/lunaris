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
| 1. Query embed | granite-embedding-311m runs **in-process** on candle (CPU) | No HTTP hop to an embedding server — the single biggest win (see the 86 ms lesson) |
| 2. Hybrid search | ONE `FT.SEARCH` HYBRID round trip; Moon fuses vector KNN + BM25 with **native RRF** server-side | Fusion happens inside the engine that owns both indices — not N queries glued together in app code |
| 2a. Filters & time | `TAG` pre-filters (`@source:{...}`) and `AS_OF <ms>` resolve **inside** the same search command | Filtering before scoring; the temporal cut never becomes an app-side post-filter |
| 3. Hydrate | Each hit's chunk row read via `read_as_of`; parent episodes batch-fetched once per unique `episode_id` | Point reads on rows the store already has hot; since-deleted chunks are skipped, not errored |
| 4. Rerank (opt-in) | bge-reranker-v2-m3 cross-encoder, in-process | ~12 ms p50 budget; only paid when you ask for it |

### The 86 ms lesson

The same 10k-document SQuAD harness (`scripts/bench-squad-kb.py`)
measured:

- **p50 86 ms** when query embedding went through an out-of-process
  Ollama HTTP server, and
- **p50 10.3 ms / p99 20.8 ms** on the strict-replay path that removes
  that hop (`scripts/ollama-replay-server.py` +
  `scripts/precompute-embeds.py`).

The engine's own search + hydrate path was ~10 ms all along; the
network hop to the embedder was ~75 ms of pure overhead. That
measurement is why v0.4 moved embedding **in-process on candle as the
default** — the shipped configuration is the configuration the
contract was proven on.

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
- Backend trade-offs (Moon vs Postgres vs SQLite):
  [Choosing a Backend](../operations/backends.md)
- The published numbers:
  [`docs/benchmarks/v0.2.x/`](https://github.com/pilotspace/lunaris/tree/main/docs/benchmarks/v0.2.x)
