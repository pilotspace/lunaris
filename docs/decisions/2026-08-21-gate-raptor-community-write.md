# ADR: Gate the RAPTOR community-tree write behind a default-OFF flag

- **Date**: 2026-08-21
- **Status**: Accepted (owner decision, ship-plan W4.5 — "gate the write, do
  not delete, do not wire a reader")
- **Owners**: Lunaris core
- **Related**: `crates/lunaris-ingest/src/pipeline.rs`
  (`RAPTOR_ENABLED_ENV_VAR`, `build_community_tree`),
  `crates/lunaris-retrieve/src/composition.rs` (`production_root`),
  `crates/lunaris-retrieve/src/operators/tree.rs` (the only reader),
  `docs/decisions/2026-08-18-graph-default-off-ga.md` (same shape, different
  subsystem), `docs/planning/2026-08-21-ship-plan.md` W4.5

## Decision

The RAPTOR community-tree write at ingest is **OFF by default**, gated by
`LUNARIS_RAPTOR_ENABLED` (truthy set `1|true|TRUE|on|ON`, identical to
`LUNARIS_GRAPH_ENABLED` and `LUNARIS_RECALL_RERANK`).

No code is deleted. No reader is wired. Setting the flag restores the
previous behaviour exactly, and the day a production reader lands, the
decision is reversed by changing one default.

## Context — the tree is written and never read

`assemble_and_write` has, since Phase 29/30, built a RAPTOR community tree on
**every ingest that reaches it**: `build_raptor_tree` →
`ExtractiveSummarizer::summarize` → `embed_batch` over the summaries →
`2 × N` `WriteOp`s into the KV space and the `communities` vector index.

"Every ingest that reaches it" is the shipped default path and then some:
`Lunaris::ingest` with the graph OFF (the default, per
`2026-08-18-graph-default-off-ga.md`) routes
`ingest_episode_with_bakeoff → assemble_and_write`, and `lunaris-hook` calls
`lunaris_ingest::ingest_episode` directly. Note the asymmetry that survived
review unnoticed: the graph-**ON** fan-out (`lunaris/src/ingest.rs::ingest_episode_graph_on`)
assembles its own `WriteOp` vector and **never wrote a community at all**.
The tree was therefore already absent from one of the two ingest paths, with
no consequence anywhere — which is the same finding as "no reader", arrived at
from the other side.

Nothing on a default path reads any of it:

- `production_root` (`composition.rs:52-59`) composes `chunks_leg`
  (`Vector("chunks") ∧ BM25("chunks")`) and `facts_leg`
  (`Navigate("entities") ∧ BM25("facts")`). `communities` appears in neither.
- The only operator that queries `communities` is `Tree`
  (`operators/tree.rs`), reachable exclusively through the opt-in
  `.tree(index, k, depth)` DSL builder (`builder.rs:295`). `Tree::new` has no
  production call site — its only callers are that builder and
  `lunaris-retrieve/tests/tree_recall.rs`.
- Every shipped surface — MCP `memory.recall`, the Claude Code hook's context
  injection, HTTP `/v1/recall`, and both SDKs — routes through
  `production_root`.
- `Chunk.parent_id`, the leaf half of the same wiring, likewise has no
  production reader: it is written by `build_raptor_tree` and read only by
  tests.

So the write is pure cost. This is the same class of defect the ship plan
files as W4.8 ("every writer has a reader"); W4.5 is the first instance
closed.

## The cost, per ingest, read off the source

Let `C` = chunks in the episode and `N` = communities, where
`N = max(1, heading count)` — `build_doctree` synthesises a root `TocNode`
when `records.is_empty()`, so **there is no document shape that produces zero
communities.**

| Resource | Cost when ON | Notes |
|---|---|---|
| LLM summarization calls | **0** | `pipeline.rs` hardcodes `ExtractiveSummarizer`. `LlmSummarizer` sits behind the `llm-summarizer` Cargo feature and has **zero** call sites in the pipeline. The tree costs no tokens on any shipped build. |
| Embedding calls | **+1 `embed_batch`** | One extra batch carrying `N` summary texts, on top of the `ceil(C/32)` chunk batches — or `N` single-input calls on the INGEST-02 fallback. In production that is a real granite-r2 forward pass per ingest. |
| `WriteOp`s | **+2 × N** | One `KvPut` (community JSON) + one `VectorUpsert` (`communities` index) per community. |
| Bytes into Moon | **≈3.1–7.4 KB per community** | `VectorUpsert` carries 768 × f32 = **3072 B** — the irreducible floor — plus metadata `{summary, level, parent}`. The `KvPut` carries the `Community` JSON (`summary_embedding` is `skip_serializing`). `summary` is capped at `MAX_SUMMARY_BYTES` = 2048 and is stored **twice**: once in the KV blob, once in the vector metadata. |
| CPU / allocation | `C × 3072 B` memcpy + `O(N × C)` text clones | `build_raptor_tree` opens with a defensive `chunks.to_vec()` that deep-clones every chunk *including its 768-d embedding*; `chunk_texts_for_community` then runs a DFS per community, cloning every descendant chunk's text. The OFF path moves `chunks` through untouched and skips both. |

**The shape that matters most is the cheapest-looking one.** A heading-free
conversational turn — what the MCP tool and the Claude Code hook actually
ingest, and the dominant shape in the owner's live personal store — yields
`N = 1`. That turn pays a full extra embedder round-trip, two `WriteOp`s and
at least 3072 bytes, to persist one community whose summary is the first
sentence of the text it just stored verbatim one key over.

Measured, not asserted:
`crates/lunaris-ingest/tests/raptor_gate.rs::gate_removes_one_embed_round_trip_and_every_community_row`
runs both arms against the same fixture and pins `OFF + 1 == ON` embed calls
and `OFF == 0` communities.

## Why a gate and not a deletion

Deleting would throw away a working, tested implementation of hierarchical
retrieval whose only defect is that its consumer was never built. The tree is
the substrate a future RAPTOR reader needs, and rebuilding it later costs more
than carrying it. A flag makes the cost stop today and keeps the option.

## Why `LUNARIS_RAPTOR_ENABLED` and not the graph umbrella

`LUNARIS_GRAPH_ENABLED` gates entity/relation/fact extraction: a different
subsystem, a different write path in `lunaris/src/ingest.rs`, and — unlike
RAPTOR — one that **has** a live reader (`facts_leg`'s `Navigate` + BM25 legs
on `production_root`). Folding RAPTOR under it would mean an operator cannot
enable the graph, which pays for itself, without also re-enabling the tree,
which does not — and vice versa. Two independent costs get two independent
switches. The idiom (const + pure `*_from_value` decision function + truthy
set) is shared, so operators only learn one convention.

## Implementation notes

- The entire RAPTOR pass moved into one `build_community_tree` function, so
  "the gate is closed" and "none of that code ran" are the same statement.
- **INGEST-04 is untouched in both directions.** The gate only decides how
  many ops enter the single `Vec<WriteOp>`; it never adds a second write path.
  `pipeline.rs` still contains exactly one `storage.atomic_write` call site.
- The flag is read per ingest, not cached at construction (matching
  `ingest_embed_batch_size`, issue #49) so long-running daemons observe
  operator changes without a restart. One env read per ingest is noise beside
  the embed call it may save.
- `ingest_episode_with_raptor` takes the flag explicitly. Tests use it rather
  than mutating process env, which edition 2024 makes `unsafe` and which races
  across parallel tests — the `RecallRerankConfig::from_values` precedent.

## Consequences

- **Anyone already driving `.tree(..)` must opt in and re-ingest.** With the
  flag off the `communities` index is never created; `Tree` already treats a
  missing index as "this scope has no communities" and returns empty rather
  than erroring (`operators/tree.rs:158-165`), so the failure mode is silent
  empty results, not a crash. Set `LUNARIS_RAPTOR_ENABLED=1` and re-ingest.
- Existing `communities` rows written before this change are left in place.
  They are inert, and `forget` does not reach them (`W1.4`).
- `Chunk.parent_id` is `None` on newly-ingested chunks unless the flag is set.
  Nothing in production reads it; the field's serde back-compat already covers
  both states (`primitives.rs::chunk_deserializes_without_parent_id`).

## Still deferred

`pipeline.rs`'s `TODO(phase-future)`: pushing `WriteOp::GraphEdge` for
parent-child edges (D7). It stays deferred, and now doubly so — a navigable
edge over a tree with no reader would have nothing to navigate for. It becomes
live work only as part of building the reader.

## Reversal criteria

Flip the default when a production recall path actually queries `communities`
— i.e. when `production_root` (or a named operating point beside it) composes
a tree leg, with a measured quality result above the ±5-pt LongMemEval
judge/gen noise floor and a latency envelope inside the sub-25 ms p50 core
contract. That flip is its own ADR, with a ratchet re-baseline, exactly as
`2026-08-18-graph-default-off-ga.md` requires for the graph.
