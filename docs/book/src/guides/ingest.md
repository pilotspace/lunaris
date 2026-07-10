# Ingesting Observations

**Reach for this chapter when you need to put data into Lunaris — durably, with
exactly one storage transaction per call.** Every write begins with
`ScopedLunaris::ingest`. One call commits one `atomic_write` on the backend,
whether the [graph pipeline](./graph.md) is on or off.

## The shape

```rust
use lunaris::{EpisodeBuilder, Lunaris, Scope};

let lunaris = Lunaris::open("postgres://lunaris@localhost/lunaris").await?;
let scope   = Scope::new("acme.agent-1")?;            // partition key — no colons
let scoped  = lunaris.scoped(scope);

let lsn = scoped
    .ingest(EpisodeBuilder::new("notes.md", "# Notes\nThe quick brown fox."))
    .await?;
```

`EpisodeBuilder` (`crates/lunaris/src/episode_builder.rs`) is a *scope-less*
payload builder — `source`, `content`, and the optional `t_ref` / `metadata`
/ `id`. The scope is stamped exactly once, by `ScopedLunaris::ingest`, via the
`pub(crate)` `EpisodeBuilder::into_episode`. Callers cannot reach around the
`ScopedLunaris` wrapper to inject an arbitrary scope — that is the type-level
guard behind [multi-agent isolation](./multi-agent.md).

| Builder method | Effect |
|---|---|
| `EpisodeBuilder::new(source, content)` | Required. `source` is the namespace-qualified origin (`"helios:fs/report.md"`, `"chat:session-42/turn-7"`); `content` is the raw text that gets chunked + embedded. |
| `.id(ulid)` | Override the auto-generated ULID — for idempotent replay / migration tooling. Default: fresh `Ulid::new()`. |
| `.t_ref(chrono::DateTime<Utc>)` | Set the valid-time anchor. Default: the engine's `HlcClock` wall time at ingest. |
| `.metadata(map)` | Merge `serde_json` key/value pairs onto the episode. |

The returned value is an `Lsn` — a **replay cursor**, not a primary key. It
tells the snapshot endpoint (`GET /v1/snapshot/{lsn}`) where to resume.
De-dupe on `Episode::id`, never on `Lsn`.

## What `ingest` does

`ScopedLunaris::ingest` stamps the scope and delegates to `Lunaris::ingest`,
which reads the graph-pipeline toggle **once** at the top and picks a branch
(`crates/lunaris/src/ingest.rs:64-152`). Either branch runs:

1. **Chunk** — `lunaris_ingest::chunk_markdown(&content, 500, 100)`: a
   markdown-aware chunker, ~500-token target, 100-token overlap, heading path
   preserved on every chunk (`crates/lunaris-ingest/src/chunker.rs`).
2. **Embed** — `embedder.embed_batch(&[..])` in batches of
   `INGEST_EMBED_BATCH_SIZE = 32`. On a batch error, it degrades to per-chunk
   single-input embeds; a per-chunk failure surfaces immediately as
   `LunarisError::Storage(Backend(_))`
   (`crates/lunaris-ingest/src/pipeline.rs:120-178`). The default embedder is
   **granite-embedding-311m-multilingual-r2** (768-d, Q4_K_M GGUF), running
   in-process via **llama.cpp** — no external service required; the GGUF is
   staged at `~/.lunaris/models/` (the MCP server stages it lazily on first
   recall; other deployments download it out-of-band). See
   [Configuration → Embedder](../reference/configuration.md#embedder-and-reranker-details).
3. **Assemble one `Vec<WriteOp>`** — one `KvPut` for the episode, plus per
   chunk a `KvPut` (chunk JSON) and a `VectorUpsert` (768-d embedding +
   `{episode_id, heading_path, offset, text, source}` metadata). The `text`
   field is what lets both Postgres and Moon BM25 score chunk content.
4. **One `atomic_write`** — `storage.atomic_write(&scope, &ops).await`. All
   chunks for an episode land or none do; that is the Phase 1 atomicity
   contract.

After commit (and only after — the data is already durable), `ingest`
fire-and-forgets one `__lunaris_consolidate__` envelope carrying
`{episode_id, lsn, source}`. A publish failure logs and continues — it never
fails the ingest. See [Consolidation & Verification](./consolidate-verify.md).

### The INGEST-04 invariant

**Exactly one `atomic_write` per ingest call.** `ingest` does not commit per
chunk — it builds the full `Vec<WriteOp>` and hands it to the backend once.
The invariant is enforced separately per branch:

- **Graph OFF** — the single call site is `crates/lunaris-ingest/src/pipeline.rs:116`.
  CI runs a grep gate on every push:
  `grep -v '^\s*//' crates/lunaris-ingest/src/pipeline.rs | grep -c 'storage\.atomic_write'`
  must equal `1` (comments mentioning `atomic_write` are stripped first).
- **Graph ON** — the single call site is in `ingest_episode_graph_on`
  (`crates/lunaris/src/ingest.rs`, the `ONE atomic_write call (INGEST-04 …)`
  comment). The extended fan-out (entities, relations, facts) extends the
  *same* `WriteOp` vector — it does not introduce a second `atomic_write`.

Any new ingest fan-out must extend the existing vector. A second
`atomic_write` is a bug.

## With the graph pipeline on

When `lunaris.graph_pipeline().enable()` has been called (or
`LUNARIS_GRAPH_ENABLED=1` was set at open time), `Lunaris::ingest` routes to
the graph-on branch:

- Each chunk is run through the [`Extractor`](./graph.md) →
  `validator::validate` → `ValidatedExtraction`. (A `NoopExtractor` —
  installed automatically when no `LUNARIS_EXTRACT_PROVIDER` is configured —
  short-circuits this: `applies() == false` skips the extract call entirely,
  so no `GraphNode`s are written.)
- The single `WriteOp` vector grows to include, per extracted entity, a
  `GraphNode` + a `VectorUpsert` into the `entities` index; per relation, a
  `GraphEdge`; per fact, a `KvPut` + a `VectorUpsert` into the `facts` index.
- After commit, every `NeedsReview` item also publishes one
  `__lunaris_verify__` message (consumed only when the
  [verifier pipeline](./consolidate-verify.md) is enabled).

A toggle change *during* an in-flight ingest takes effect on the **next** call
— never mid-call.

## Gotchas

- **Bare `Lunaris::ingest(Episode)` still exists** but the scoped path is the
  one to use. The HTTP server already routes every `POST /v1/ingest` through
  `ScopedLunaris::ingest` keyed on the JWT `tenant` claim.
- **GGUF staging.** The default embedder (llama.cpp granite-r2) expects its
  GGUF at `~/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf`
  — there is **no auto-download** in the umbrella crate; a missing GGUF logs a
  `WARN` and falls back to `NoopEmbedder`. The MCP server stages GGUFs lazily
  on first recall. Point `LUNARIS_EMBEDDER_GGUF` at an existing local copy, or
  build with `--features embed-remote` and set `LUNARIS_EMBEDDER_OLLAMA_URL`
  to use the Ollama HTTP escape hatch (resolves after the llama.cpp step).
- **Embedding dimension.** The Moon adapter creates its vector index at the
  configured embedder's dimension (default 768-d; `Lunaris::open` passes
  `embedder.dim()`, or use `MoonStorage::connect_with_dim` directly), so a
  1536-d embedder works on Moon out of the box. pgvector handles up to
  ~1536-d. Footgun: Moon's `FT.CREATE` won't resize an existing index — drop
  it first if you switch embedder width. See
  [Choosing a Backend](../operations/backends.md).
- **Higher-level wrappers exist.** If you don't want to hand-build episodes,
  the [Cookbook](../cookbook/index.md) recipes (`DocumentKnowledgeBase`,
  `ChatAgentMemory`, …) forward to `ingest` with opinionated `source`
  prefixes.

## See also

- [The Retrieval DSL](./retrieval-dsl.md) — reading the data back.
- [Forgetting](./forget.md) — taking it out again.
- [Configuration Reference](../reference/configuration.md) — every feature
  flag and `LUNARIS_*` env var.
