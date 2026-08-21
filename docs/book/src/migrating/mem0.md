# Migrating from Mem0

> Adapted from `docs/MIGRATING-FROM-MEM0.md` (kept in the repo as the
> standalone version).

Lunaris and Mem0 occupy adjacent niches in the agent-memory space.
This page maps Mem0 concepts to their Lunaris equivalents so a team
already running Mem0 can evaluate the switch with concrete code,
not marketing comparisons.

> **TL;DR** — if your agent needs sub-25 ms recall with provable
> all-or-nothing commits across vector + graph + keyword + audit, the
> Mem0 fan-out architecture cannot give you that guarantee.
> If your agent needs a hosted SaaS with zero infra and minute-scale
> recall latency, stay on Mem0. The two tools answer different
> questions.

## At a glance

| Concern                                    | Mem0                                    | Lunaris                                                |
|---------------------------------------------|------------------------------------------|--------------------------------------------------------|
| **Runtime**                                | Python / hosted REST API                | Rust core + Python (PyO3) + TypeScript (NAPI) bindings |
| **Storage**                                | Vector DB + graph DB + relational (3 services) | Moon (one substrate, FT.* + graph + KV native) |
| **Atomicity**                              | Best-effort per-store; no cross-store transaction | One `atomic_write` covers vector + KV + BM25 + audit + queue. CI gate enforces single call site |
| **Bi-temporal facts**                      | Not modeled — overwrite semantics       | First-class `(valid_time, sys_time)` tuple per row     |
| **Recall latency (laptop, 1M facts)**     | p95 ~1.44 s (Mem0-published "selective" figure, 2026-06; wide query-dependent range) | **p50 19.2–22.4 ms / p99 23.4–24.4 ms** measured at **100k documents per scope** (not 1M — see note below): single-shard Moon v0.8.5, Apple M4 Pro, graph OFF, rerank OFF, k=30, retrieval-only, manual bench (not CI-gated) — [`capacity.md`](https://github.com/pilotspace/lunaris/blob/main/docs/operations/capacity.md). Budget p50 ≤ 25 ms / p99 ≤ 100 ms |
| **Tenancy**                                | Per-user "user_id" string               | `Scope` newtype with regex-validated alphabet `[A-Za-z0-9_\-.]{1,128}`, propagated through every storage call and enforced by a per-scope Moon keyspace |
| **Forgetting**                             | Hard delete                              | Tombstone via bi-temporal `sys_time` close (audit trail preserved) |
| **License**                                | Apache 2.0                              | Apache 2.0                                             |
| **Default LLM coupling**                  | OpenAI                                   | None — extractor/verifier are remote-only (anthropic/openai/gemini/minimax/openai-compat) or a custom impl; both optional |

## Code-side comparison

### Ingest one observation

**Mem0**

```python
from mem0 import Memory
m = Memory()
m.add(
    messages=[{"role": "user", "content": "Alice joined Acme on 2024-04-01."}],
    user_id="alice",
)
```

**Lunaris** (Python — equivalent surface)

```python
import lunaris

mem = lunaris.Lunaris.open("moon://localhost:6380")
scope = lunaris.Scope("user.alice")
mem.scoped(scope).ingest(
    lunaris.EpisodeBuilder("chat:session-1/turn-1",
                            "Alice joined Acme on 2024-04-01.")
)
```

Two notable differences:

1. **`scope` is a typed object, not a string.** Wire-side payloads
   cannot inject a different scope past the type system — the
   `ScopedLunaris` wrapper threads the validated `Scope` through every
   storage call. (The typed `Scope` / `EpisodeBuilder` SDK ergonomics
   land in v0.3; today the Python surface uses dicts — see the
   [Python SDK](../sdk/python.md) page.)
2. **The ingest call returns an `Lsn` (Log Sequence Number)** so you
   can wait on the audit envelope before responding to the user
   — useful when the agent's reply depends on a fact being committed.

### Recall

**Mem0**

```python
hits = m.search(query="when did Alice join Acme?", user_id="alice")
# returns a list of dicts; no fusion control, no time-travel.
```

**Lunaris**

```python
hits = await (
    mem.scoped(scope)
       .recall()                              # pre-bound builder, default root Vector("chunks", 30)
       .and_(lunaris.Keyword.bm25("chunks", 30))
       .fuse_rrf(60)
       .top(5)
       .execute()                             # plan collapses to one FFI call; no query-text arg yet
)
# hits is List[Hit]; each Hit carries score (RRF-fused), raw_score,
# content, source, valid_time, sys_time, degraded (bool).
```

The composable retrieval DSL means you opt into hybrid search,
re-rank, graph traversal, or time-travel — each is one combinator,
and the type system rejects mixing incompatible operators. See
[The Retrieval DSL](../guides/retrieval-dsl.md).

### Time-travel recall (no Mem0 equivalent)

> **Backend note (v0.6.2).** `.as_of(<past timestamp>)` needs a backend that
> keeps a KV version chain to hydrate the historical rows, and **no 0.7.0
> backend does**: the call returns `StorageError::NotSupported` (HTTP
> `501 not_supported`). Moon stores Lunaris rows as plain hashes and refuses a
> historical pin rather than silently answering with present-time data; the
> Postgres and SQLite backends that answered it were deleted in 0.7.0. The
> search and graph lanes stay temporal (`FT.SEARCH AS_OF`,
> `GRAPH.QUERY VALID_AT`).

```python
from datetime import datetime, timezone

snapshot_ms = int(datetime(2024, 6, 1, tzinfo=timezone.utc).timestamp() * 1000)
hits = await (
    mem.scoped(scope)
       .recall()                   # default root Vector("chunks", 30)
       .as_of(snapshot_ms)         # ← bi-temporal cut (ms since the Unix epoch)
       .execute()
)
# Returns facts as they were known on 2024-06-01. Later updates
# (corrections, retractions) are invisible to this query.
```

### Forget a fact (audit trail preserved)

**Mem0**

```python
m.delete(memory_id="<uuid>")
# Row is gone; no record that it ever existed.
```

**Lunaris**

```python
await mem.forget(lunaris.ForgetTarget.episode_id(episode_id))
# Closes sys_time on the row(s). Audit log records the close.
# Time-travel queries with as_of < forget_ts still see the row.
```

This is why GDPR / SOC2 audits prefer the bi-temporal model — you
can prove "this fact was retracted at time T" without throwing
away the evidence that it ever existed. See [Forgetting](../guides/forget.md).

## Migration checklist

A team running Mem0 in production typically migrates incrementally.
Suggested phases:

1. **Stand up Lunaris alongside Mem0.** Use the `examples/quickstart-py`
   recipe — 5-minute docker-compose, no infra commitment.
2. **Mirror writes.** Every `m.add(...)` is also dispatched to
   `mem.scoped(scope).ingest(...)`. Lunaris is now collecting
   bi-temporal facts in parallel.
3. **Shadow reads.** Every `m.search(...)` is also issued to
   Lunaris's recall DSL. Diff the result sets in your eval harness.
4. **Cutover when the diff is acceptable.** Promote Lunaris to
   primary; keep Mem0 as fallback for ~1 release.
5. **Decommission Mem0.** At this point you own one Rust process
   instead of three Python services. Recall p50 measures **19–22 ms** on a
   100k-document scope ([`capacity.md`](https://github.com/pilotspace/lunaris/blob/main/docs/operations/capacity.md) — manual
   bench, not CI-gated); measure your own Mem0 baseline (Mem0's published figure is
   a p95 ~1.44 s, selective).

## When NOT to migrate

If any of these is true, **stay on Mem0**:

- You need a hosted SaaS with zero infra ownership.
- Recall latency in the 100–500 ms range is fine.
- You don't need bi-temporal queries (every fact is "current").
- You're building a single-user, single-tenant prototype where
  `Scope` would be overhead.
- Your stack is pure Python and adding a Rust binary to the deploy
  pipeline is more friction than it's worth.

Lunaris is built for production agent platforms — internal-first,
performance-bound, audit-bound. Mem0 is built for developer
productivity at the prototype stage. Use the right tool.

## Open questions / known gaps

- **Bulk import.** No first-class `m.add_many(messages)` analogue
  yet; v0.3 will add a bulk-ingest path that amortises the embed
  + atomic_write call. Today you call `ingest()` in a loop.
- **Mem0's metadata-extraction prompts.** Lunaris ships an
  Extractor trait with remote-only backends (`ollama` /
  `cloud-api`, selected via `LUNARIS_EXTRACT_PROVIDER`), but the default
  prompt set differs from Mem0's. If your existing Mem0 deployment depends
  on specific extracted fields, override `Extractor::extract` and port your
  prompt.
- **Graph queries.** Mem0 OSS v3 removed graph support — it is now
  Platform-only (Mem0g, Neo4j-backed, with an LLM on the read path and
  open deletion bugs). Lunaris's `Graph::anchored(entity_ids, hops)` is
  an opt-in operator, off by default, with no LLM on the read path. Both
  require the entity-resolution Extractor pipeline to have populated
  `(entity, relation)` triples first.

See `docs/RELEASE.md` for the current v0.2.x release scope and what
lands in v0.3. The [Zep](./zep.md) and [Cognee](./cognee.md) pages
cover the parallel migration stories.
