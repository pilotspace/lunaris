# Core Concepts

**The mental model behind Lunaris: you write `Episode`s, ingest turns
them into stored primitives, recall composes a read plan over them — and
every layer is bi-temporal, scope-partitioned, and committed in one
atomic write.** Read this once and the rest of the book is configuration
detail.

## The flow: Episode → ingest → storage → recall

```
          Episode
             |
             v
      +-------------+        (optional)         +----------------+
      |   ingest    | -- graph pipeline ON --> |  extract       |
      |  (chunk,    |                          |  entities +    |
      |   embed,    |                          |  relations +   |
      |   atomic    |                          |  facts         |
      |   write)    | <---- validator --------+----------------+
      +------+------+
             |                                            ^
             v                                            |
      +-------------+                             +--------------+
      |   storage   | <-- MVCC supersede -------> | forget       |
      | (KV/Vector/ |       (soft/hard)           | (GDPR/audit) |
      |   Graph/    |                             +--------------+
      |   Queue)    |
      +------+------+
             |
   recall    v        rerank
   builder -+-> [vector] ----\                       queue
            +-> [bm25 ]  ---- fuse_rrf --> rerank --> Hit[]
            +-> [graph] ----/                          |
                                                       v
                                               +-------+-------+
                                               | __lunaris_    |
                                               |  verify__     |  <-- verifier worker (default OFF)
                                               | __lunaris_    |
                                               |  consolidate__|  <-- consolidator worker (default OFF)
                                               | __lunaris_    |
                                               |  audit__      |  <-- audit sink
                                               +---------------+
```

- **`ingest`** chunks the episode body (markdown-aware, ~500-token
  target with ~100-token overlap), embeds each chunk in batches, and
  commits everything — episode KV row, per-chunk KV rows, per-chunk
  vector upserts, plus graph/fact rows if the graph pipeline is on — in
  a **single** `StoragePort::atomic_write` call. Then it publishes one
  message to `__lunaris_consolidate__` (best-effort; the data is already
  durable).
- **`forget`** is the inverse: it also commits in one `atomic_write`
  (soft delete stamps the MVCC `sys` end; hard delete is a `KvDelete`
  fan-out behind a two-step confirmation token), and emits one audit
  event.
- **`recall`** is read-only: a `RetrievalBuilder` composes `Vector`,
  `Keyword`, and `Graph` operators (plus fusion / rerank / fallback
  wrappers) into a plan, executes it once, and returns `Vec<Hit>`. No
  LLM is on this path — that's the sub-25 ms moat.

## The primitives

Six primitive types, each carrying a `BiTemporal` stamp and a `Scope`
(`crates/lunaris-core/src/primitives.rs`):

| Primitive | What it is | Produced by |
|---|---|---|
| **`Episode`** | A raw observation: chat turn, document, tool output. `{ id, scope, source, content, t_ref?, bt, metadata }`. The only thing you write directly. | you, via `EpisodeBuilder` |
| **`Chunk`** | A retrieval-sized slice of an episode: `{ id, scope, episode_id, text, tokens, offset, heading_path, overlap_tail, embedding?, bt }`. The unit vector search ranks. | `ingest` (markdown chunker) |
| **`Entity`** | A named thing — `{ id, scope, name, aliases, entity_type, embedding?, bt, confidence }`. | the extractor (graph pipeline) |
| **`Relation`** | A typed edge between two entities. | the extractor |
| **`Fact`** | An extracted assertion, contradiction-checked by the validator. | the extractor + validator |
| **`Community`** | A Leiden-detected cluster of entities, surfaced by the consolidator. | the consolidator |

Only `Episode` is written by your code; everything below it is derived.
Recall queries one of four whitelisted index names — `chunks`,
`entities`, `facts`, `communities` — e.g. `Vector::new("chunks", 30)`.

## Bi-temporal MVCC + the HLC

Every primitive carries a `BiTemporal` (`crates/lunaris-core/src/bitemporal.rs`):

```rust
pub struct BiTemporal {
    pub valid: (Hlc, Option<Hlc>),  // when the fact is true in the world
    pub sys:   (Hlc, Option<Hlc>),  // when the system observed/recorded it
}
```

Both are half-open `[from, to)` intervals; `to = None` means "still
current". `valid` is *world time* — "Alice loved chocolate from Jan to
March". `sys` is *system time* — "we recorded that on Feb 3rd, and
superseded it on April 1st". The two axes are independent, which is what
lets you ask **"what did the agent believe at time T?"** as a query
(`.as_of(ts)` on the retrieval builder, or `read_as_of` on the storage
port) rather than reconstructing it from a log.

Updates are **MVCC supersede**, never in-place mutation: a soft delete
or an arbitration outcome stamps the *old* row's `sys` end and writes a
*new* row — old `as_of` reads still resolve correctly. (Backends derive
persisted bitemporal from the payload bytes, so a typed-only mutation
would be silently lost; the supersede writers patch both.)

The clock is a **Hybrid Logical Clock** (`crates/lunaris-core/src/hlc.rs`):
`Hlc { wall_ms: u64, counter: u32, node_id: u16 }`, totally ordered by
`(wall_ms, counter, node_id)`. When the wall clock doesn't advance
between two ticks, the `counter` increments — so no two stamps ever
collide, even under burst writes. `node_id` is `0` in single-node v0.

## Scope: the multi-agent partition key (RFC 0001)

Every Lunaris operation is partitioned by a **`Scope`** — a validated
newtype (`crates/lunaris-core/src/scope.rs`), not a `user_id` string the
caller could swap:

```rust
let scope  = Scope::new("acme.agent-1")?;   // validates against [A-Za-z0-9_\-.]{1,128}
let scoped = lunaris.scoped(scope);          // ScopedLunaris — every op partitioned
scoped.ingest(EpisodeBuilder::new("user-msg", "Alice loves chocolate.")).await?;
```

`Scope::new` rejects the empty string, anything over 128 bytes, and any
character outside `[A-Za-z0-9_\-.]`. **`:` is not in the alphabet** — by
design, so the KV key format can't byte-alias across scopes (see below).
`Scope` does *not* derive `Deserialize` transparently — the hand-rolled
impl re-runs the validating constructor, so the wire can't smuggle an
invalid or unintended scope past the type. (`Scope::dev()` exists but is
`#[doc(hidden)]` — a migration crutch for code paths that haven't
threaded a real scope through yet.)

On the **HTTP** surface, the `tenant` claim from the bearer token is the
*only* source of truth for the partition scope — route handlers ignore
any `scope` field on the request body. On **Postgres**, row-level
security re-enforces it at the database boundary (both `USING` and
`WITH CHECK`, under a `NOSUPERUSER NOBYPASSRLS` role). Same ULID under
two different scopes = two distinct rows, no leak. Details:
[Multi-Agent & Scope](../guides/multi-agent.md).

### The keyspace

KV keys are minted exclusively by `lunaris_core::keyspace` —
`episode_key`, `chunk_key`, `entity_key`, `relation_key`, `fact_key`,
`community_key` — in the canonical format:

```
lunaris:{scope}:{kind}:{ulid}
```

e.g. `lunaris:acme.agent-1:chunk:01J...`. Backend crates re-export these
helpers; minting a Lunaris KV key from a local helper is a bug. Because
`:` is forbidden in `Scope`, the `{scope}` segment can never absorb the
`:{kind}` delimiter — scope isolation is closed at the type level, not
by operator discipline.

## The single `atomic_write` invariant (INGEST-04)

This is the correctness moat. One `ingest` call = exactly one
`StoragePort::atomic_write` — never one commit per chunk. The pipeline
builds the full `Vec<WriteOp>` (episode KV + per-chunk KV + per-chunk
vector upsert, plus graph/fact rows when the graph pipeline is on) and
hands it to the backend in a single call. Either all of it lands or none
of it does — across the vector index, the KV store, the BM25 index, the
audit log, and the queue. Fan-out architectures (write to the vector DB,
then the store, then the graph) can't make that guarantee; Lunaris can
because it owns the substrate.

It's grep-pinned and CI gates on it on every push: after stripping comment
lines, exactly one real `storage.atomic_write` call site must remain in
`crates/lunaris-ingest/src/pipeline.rs`
(`grep -v '^\s*//' crates/lunaris-ingest/src/pipeline.rs | grep -c 'storage\.atomic_write'`
== `1`) — see [Ingesting Observations](../guides/ingest.md#the-ingest-04-invariant)
for the exact gate and the graph-on counterpart. Any new ingest fan-out extends
the single `WriteOp` vector — it does not add a second `atomic_write`. `forget`
holds the same property: one `atomic_write` per call (zero for a
dry-run preview).

## Opt-in pipelines: graph, verify, consolidate

Three background pipelines, all **default OFF** — your dev box doesn't
download a Gemma extractor until you ask:

| Pipeline | What it does | Enable |
|---|---|---|
| **Graph** | Extract entities / relations / facts from each ingested episode; populate the graph index so `Graph::anchored(...)` recall works. | `lunaris.graph_pipeline().enable()` or `LUNARIS_GRAPH_ENABLED=1` |
| **Verify** | Slow-path arbitration: consume `__lunaris_verify__` items, resolve contradictions, MVCC-supersede the loser (RFC 0006). Pluggable model: Gemma-3-27B / -270M (laptop floor) / Ollama / cloud. | `lunaris.verify_pipeline().enable()` or `LUNARIS_VERIFY_ENABLED=1` |
| **Consolidate** | ACT-R base-level activation (Anderson 1996) + Leiden community detection; consume one `__lunaris_consolidate__` message per ingest commit, promote/archive memories. | `lunaris.consolidator_pipeline().enable()` or `LUNARIS_CONSOLIDATE_ENABLED=1` |

Each handle is obtained from the `Lunaris` value after `open`. The
consolidator handle additionally exposes `.enable_for_scope(prefix)`
(a `source`-prefix filter) for per-scope rollout; the graph and verify
handles are `.enable()` / `.disable()` only in v0.2.x. Enabling a
pipeline with no real backend wired just runs the shipped `Noop*` impl
— no crash, but no useful work either. Details:
[The Graph Pipeline](../guides/graph.md) and
[Consolidation & Verification](../guides/consolidate-verify.md).

## The two backends

`Lunaris::open(url)` dispatches on the URL scheme:

| Scheme | Backend | Notes |
|---|---|---|
| `moon://host:port` | **Moon** (Redis-compatible substrate) | Native `FT.SEARCH` (vector + BM25), `GRAPH.QUERY`, message queue, **native RRF fusion** (one round trip for `vector.and(keyword).fuse_rrf()`). The Moon adapter sizes its vector index to the configured embedder (default 768-d; a wider embedder works on Moon too via `Lunaris::open` / `connect_with_dim`). |
| `postgres://` / `postgresql://` | **Postgres** + `pgvector` + Apache AGE + `pgmq` | Native graph + queue; **client-side** RRF fusion. pgvector handles embeddings up to ~1536-d. RLS-enforced tenant isolation. The portable default. |
| anything else | — | `StorageError::UnsupportedScheme` |

Every `Lunaris` call works identically against either — the scheme is
the only thing that changes. Moon is the performance ceiling (we own
it); Postgres is the portability proof (you probably already run one).
Trade-offs and the embedding-dimension details:
[Choosing a Backend](../operations/backends.md).

## Next

- [The Retrieval DSL](../guides/retrieval-dsl.md) — every operator and
  combinator.
- [Ingesting Observations](../guides/ingest.md) — the chunker, the
  embedder driver, the graph branch.
- [Durability & Recovery](../operations/durability.md) — how the
  bi-temporal store survives a crash.
- [Configuration Reference](../reference/configuration.md) — every
  feature flag and `LUNARIS_*` variable.

> Where this chapter disagrees with the Rust source, the source wins —
> the `path:line` references above point back into the crates.
