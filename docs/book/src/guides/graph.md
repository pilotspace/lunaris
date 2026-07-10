# The Graph Pipeline (opt-in)

**Reach for this chapter when you want to follow relations out from a known
entity** — "everything we know about Alice that also matches this query". The
graph is **default OFF** (blueprint §5.2). Turning it on makes
[ingest](./ingest.md) also extract entities, relations, and facts with a small
local LLM, and unlocks the `Graph::anchored` [retrieval operator](./retrieval-dsl.md).

## Turning it on

```rust
use lunaris::Lunaris;

let lunaris = Lunaris::open("moon://localhost:6380").await?;

// Runtime toggle — idempotent.
lunaris.graph_pipeline().enable();
// ... or LUNARIS_GRAPH_ENABLED=1 seeds the initial state at open time.
```

`GraphPipelineHandle` (`crates/lunaris/src/graph_pipeline.rs`) exposes
`enable()` / `disable()` / `is_enabled()`, plus `set_extractor(arc)` /
`snapshot_extractor()`. The enabled bit is read **once per ingest call** — a
toggle change during an in-flight ingest takes effect on the next call. There
is **no per-scope `enable_for_scope` on the graph handle** (only the
[consolidator handle](./consolidate-verify.md) has that).

| Knob | Where |
|---|---|
| `lunaris.graph_pipeline().enable()` / `.disable()` | programmatic, idempotent |
| `LUNARIS_GRAPH_ENABLED` (bool: `1`/`true`/`on`) | seeds initial state at `Lunaris::open` |
| `lunaris.with_extractor(Arc::new(...))` | swap the extractor backend; apply *before* `.enable()` |

See [Configuration → Backend / pipeline selection](../reference/configuration.md#backend--pipeline-selection).

## What extraction produces

With the pipeline on, the graph-on ingest branch runs each chunk through the
[`Extractor`](#extractor-tiers-rfc-0004) → `validator::validate` →
`ValidatedExtraction`, then extends the **single** `WriteOp` vector (the
INGEST-04 invariant still holds — one `atomic_write` per ingest call) with:

- per **entity** — a `GraphNode` (props include `id_hex`) + a `VectorUpsert`
  into the `entities` index;
- per **relation** — a `GraphEdge`;
- per **fact** — a `KvPut` + a `VectorUpsert` into the `facts` index.

`EntityId` is deterministic: the 16-byte truncation of
`blake3(normalized_name || "::" || entity_type)` — stable across re-ingest,
across chunks, across episodes. No second-pass dedupe round trip.
(`crates/lunaris-extract/src/lib.rs`, `types::EntityId::from_name_and_type`.)

The validator routes invalid items to a `NeedsReview` queue with one of four
structured reasons — `InvalidBitemporal`, `StructuralContradiction` (same
`(subject, predicate)` with overlapping validity and conflicting object,
within one episode), `GbnfFailure`, `TransientAfterRetry`. After the
`atomic_write`, every `NeedsReview` item publishes one `__lunaris_verify__`
message; it is only consumed when the [verifier pipeline](./consolidate-verify.md)
is enabled. Cross-episode contradictions are deferred to the verifier.

## Graph-aware recall

Once entities/relations have been written, compose `Graph::anchored` into the
DSL:

```rust
use lunaris::{EntityId, Graph, Query, Scope, Vector};

let scoped = lunaris.scoped(Scope::new("acme.agent-1")?);
let alice  = EntityId::from_name_and_type("Alice", "Person");

let hits = scoped
    .dsl()
    .with_root(
        Vector::new("chunks", 30)
            .and(Graph::anchored(vec![alice], 2))
            .fuse_rrf(60)
            .top(30),
    )
    .rerank(lunaris.reranker())
    .top(5)
    .execute(Query::text("Tell me about Alice"))
    .await?;
```

- `hops` is clamped to `[1, MAX_GRAPH_HOPS = 5]`; `DEFAULT_GRAPH_HOPS = 2`.
- Empty `entity_ids` short-circuits to an empty result without touching
  storage.
- `.with_k(n)` caps the candidate set (`DEFAULT_GRAPH_K = 30`);
  `.with_graph(name)` overrides the graph key (default `lunaris_graph`).
- **Any `Graph` branch forces client-side RRF** — the Moon-native one-trip
  fusion path only fires for `Vector + Keyword(BM25)` on the same index.

## Extractor tiers (RFC 0004, superseded by the v0.6 llama.cpp-only cutover)

RFC 0004 originally defined an in-process candle extractor tier (Medium:
Gemma-3-4B) alongside Ollama and cloud-API backends. The candle tier was
deleted in the v0.6 llama.cpp-only cutover
(`docs/decisions/2026-07-10-llamacpp-only-cutover.md`) — the extractor is now
**remote-only**, plus a `NoopExtractor` fallback:

| Backend | Selector | Notes |
|---|---|---|
| `OllamaExtractor` | Cargo feature `ollama`, used via `with_extractor` or `LUNARIS_EXTRACT_PROVIDER=openai-compat` | POSTs `/api/chat` (or the OpenAI-compatible endpoint) with a JSON-schema `format` field. |
| Cloud-API extractor | `LUNARIS_EXTRACT_PROVIDER` = `anthropic`\|`openai`\|`gemini`\|`minimax` (Cargo feature `cloud-api`) | Single retry on transient errors then a sentinel that the validator routes to `TransientAfterRetry`. |
| `NoopExtractor` | always available | `applies() == false`; installed automatically when no extract provider is configured. |

(`crates/lunaris-extract/src/lib.rs`; RFC 0004 "extractor tiers" — historical,
its candle tier no longer exists.)

## Gotchas

- **The extractor is required for graph ingest.** With no
  `LUNARIS_EXTRACT_PROVIDER` set (and no `with_extractor` override),
  `Lunaris::open` substitutes `NoopExtractor` and emits a `tracing::warn!` —
  in that state `graph_pipeline().enable()` is a no-op and zero `GraphNode`s
  get written. Fix by setting `LUNARIS_EXTRACT_PROVIDER` (e.g. `minimax`, or
  `openai-compat` pointed at a local Ollama/llama-server) or supplying a
  custom `with_extractor` impl.
- **`communities` stays empty** until the [consolidator](./consolidate-verify.md)'s
  Leiden community-detection run lands (v1) — recall over that index returns
  nothing meanwhile.
- **Apply `with_extractor` before `.enable()`** — `.enable()` snapshots the
  current extractor; a swap afterwards propagates via `set_extractor`.
- **Cross-scope graph references are disallowed by construction** —
  `Relation.src` / `Relation.dst` must resolve within the same scope (RFC
  0001 §2.3).

## See also

- [The Retrieval DSL](./retrieval-dsl.md) — the full `Graph::anchored`
  surface.
- [Consolidation & Verification](./consolidate-verify.md) — what consumes the
  `__lunaris_verify__` messages this pipeline emits.
- [Configuration Reference](../reference/configuration.md) — extractor feature
  flags and `LUNARIS_*` env vars.
