# Migrating from Zep to Lunaris

Zep and Lunaris both model agent memory bi-temporally — they're the
closest comparison in the space. The difference is the substrate:
Zep is a hosted Python service backed by Postgres + Neo4j; Lunaris
is an embedded Rust core with a Moon backing
that runs in-process with your agent.

This doc maps Zep concepts to their Lunaris equivalents so a team
already running Zep can evaluate the switch with concrete code.

> **TL;DR** — if your agent needs hosted SaaS memory with a managed
> Knowledge Graph and accepts 200–500 ms recall latency, Zep is
> production-ready and well-documented.
> If your agent needs sub-25 ms recall, embedded deployment, or a
> Rust-native stack with no external graph service, Lunaris's
> single-substrate architecture is a meaningful simplification.

## At a glance

| Concern                                   | Zep                                          | Lunaris                                                |
|--------------------------------------------|----------------------------------------------|--------------------------------------------------------|
| **Runtime**                                | Python service (Zep Cloud or self-hosted)    | Embedded Rust core + Python (PyO3) + TypeScript (NAPI) bindings |
| **Storage**                                | Postgres + Neo4j (two services)              | Moon — one substrate, FT.* + graph + KV native |
| **Bi-temporal model**                     | Temporal Knowledge Graph — facts carry validity periods | `(valid_time, sys_time)` tuple per row — Snodgrass bi-temporal **at the storage model**. Read the scope before migrating: as-of *reads* work on the search and graph lanes (`FT.SEARCH AS_OF`, `GRAPH.QUERY VALID_AT`); a historical **KV** read has no version chain to walk on Moon, so `read_as_of` beyond a 1-hour live window **refuses** (`NotSupported` → HTTP 501) rather than answering with today's data |
| **Recall latency**                         | 200–500 ms (HTTP hop + Python)              | p50 ≤ 25 ms / p99 ≤ 100 ms on `laptop-arm64`           |
| **Tenancy**                                | `user_id` / `session_id` strings on the API | `Scope` newtype (`[A-Za-z0-9_\-.]{1,128}`) threaded through every storage call + per-scope Moon keyspace |
| **Atomicity**                              | Per-store best-effort; no cross-store transaction | One `atomic_write` covers vector + KV + BM25 + audit + queue. CI gate enforces single call site |
| **Graph queries**                          | Cypher via Neo4j                             | Moon native graph via the `Graph::anchored` operator |
| **Embedder coupling**                     | OpenAI default                                | In-process native granite-r2 (local, 768-d) by default; Ollama HTTP escape hatch (`--features embed-remote`) for air-gapped/remote deployments |
| **Memory consolidation**                  | "MemGPT-style" salience-weighted episodic   | ACT-R base-level activation + Leiden community detection (RFC blueprint §5.1) |
| **License**                                | Apache 2.0                                   | Apache 2.0                                             |

## Where Zep and Lunaris differ in spirit

Zep is **service-oriented**: your agent talks to Zep over HTTP, and
Zep owns Postgres + Neo4j behind a managed API. The strength is
clean separation; the cost is one network hop per recall, two if
you fan out to graph + vector.

Lunaris is **library-oriented**: your agent links the Rust crate
(or imports the PyO3 / NAPI binding) and the recall happens
in-process. The strength is sub-25 ms p50 and a single substrate to
operate; the cost is operating Moon yourself.

Either choice can be right. Zep is the right choice if your team
treats memory as someone else's problem to operate. Lunaris is the
right choice if your team treats memory as a hot-path performance
contract and wants library control.

## Code-side comparison

### Add a conversational turn

**Zep**

```python
from zep_python import ZepClient, Memory, Message

client = ZepClient(api_key="...")
memory = Memory(
    messages=[Message(role="user", content="Alice joined Acme on 2024-04-01.")]
)
client.memory.add_memory(session_id="alice-session-1", memory=memory)
```

**Lunaris**

```python
import lunaris

mem = lunaris.Lunaris.open("moon://localhost:6380")
scope = lunaris.Scope("alice-session-1")
mem.scoped(scope).ingest(
    lunaris.EpisodeBuilder("chat:session-1/turn-1",
                            "Alice joined Acme on 2024-04-01.")
)
```

Zep's `session_id` maps to Lunaris's `Scope`. The Lunaris type
system guarantees the scope can't be smuggled past the API — every
storage call takes `&Scope`, and Moon bakes it into the key, the FT index
name, and the graph name.

### Recall — semantic

**Zep**

```python
result = client.memory.search_memory(
    session_id="alice-session-1",
    text="when did Alice join Acme?",
)
# result.facts: List[Fact]; each Fact has content, valid_at, invalid_at.
```

**Lunaris**

```python
hits = await (
    mem.scoped(scope)
       .recall()
       .vector("chunks", 30)
       .top(5)
       .execute(lunaris.Query.text("when did Alice join Acme?"))
)
# hits is List[Hit]; each Hit has content, valid_time, sys_time, score, degraded.
```

### Recall — hybrid (vector + BM25 + RRF fusion)

Zep does not expose hybrid retrieval as a first-class API — you
get semantic search; keyword fall-back is up to you.

**Lunaris**

```python
hits = await (
    mem.scoped(scope)
       .recall()
       .vector("chunks", 30)
       .and_(lunaris.Keyword.bm25("chunks", 30))
       .fuse_rrf(60)              # Reciprocal Rank Fusion, k=60
       .top(5)
       .execute(lunaris.Query.text("when did Alice join Acme?"))
)
```

When your query contains a capitalized proper noun (e.g., "Acme"),
the BM25 branch tends to outscore vector — RRF fusion catches that
without you having to write a router.

### Time-travel recall

> **Backend note (v0.6.2).** `.as_of(<past timestamp>)` needs a backend that
> keeps a KV version chain to hydrate the historical rows, and the two
> backends that did (Postgres, SQLite) were deleted in 0.7.0. On **Moon** the call returns
> `StorageError::NotSupported` (HTTP `501 not_supported`) — Moon stores
> Lunaris rows as plain hashes, and since v0.6.2 it refuses a historical pin
> rather than silently answering with present-time data. Moon's search and
> graph lanes stay temporal (`FT.SEARCH AS_OF`, `GRAPH.QUERY VALID_AT`).

**Zep**

```python
# Zep's facts carry valid_at / invalid_at; you can filter post-hoc:
result = client.memory.search_memory(session_id="alice", text="...")
fresh = [f for f in result.facts if f.valid_at <= snapshot_ts and (f.invalid_at is None or f.invalid_at > snapshot_ts)]
```

**Lunaris**

```python
hits = await (
    mem.scoped(scope)
       .recall()
       .vector("chunks", 10)
       .as_of(snapshot_ts)        # ← bi-temporal cut at the storage layer
       .execute(lunaris.Query.text("..."))
)
```

The Zep approach pulls every fact then filters in Python; Lunaris
pushes the temporal cut into the storage query (`FT.SEARCH AS_OF` on
Moon). On 1M-fact corpora the
latency difference is meaningful.

### Graph traversal

**Zep**

```python
# Knowledge Graph is exposed via search_memory; can't anchor traversal
# from a specific entity programmatically without dropping to Neo4j.
```

**Lunaris** — *Rust only today.* `Graph::anchored` composes into the operator
tree and resolves to a native Moon graph query:

```rust
let hits = mem.scoped(scope)
    .dsl(Graph::anchored(vec![alice_id], 2).and(Vector::new("chunks", 30)).top(10))
    .execute(Query::text("who does Alice work with?"))
    .await?;
```

> **The Python and TypeScript SDKs cannot run this yet.** Their `.execute()`
> collapses the operator tree to a single `(index, k, query)` triple before
> crossing the FFI, and a graph leg has no field in that shape. Earlier drafts
> of this guide showed a `.recall().graph(...)` chain — no such method exists
> on the Python builder, and composing `Graph.anchored(...)` in via `.and_()`
> used to drop the traversal silently and hand back a plain vector recall. Both
> SDKs now raise `NotImplementedError` / `Error` rather than answer a different
> question; use the Rust API for graph traversal until the FFI carries the leg
> (ship-plan F2).

### Forget

**Zep**

```python
client.memory.delete_memory(session_id="alice-session-1")
# Hard delete — session and its memories gone.
```

**Lunaris**

```python
await mem.forget(lunaris.ForgetTarget.episode_id(episode_id))
# Closes sys_time on the affected rows. Audit log records the close.
# Time-travel queries with as_of < forget_ts still see the row.
```

This is the GDPR/SOC2-friendly shape: the fact "we retracted this
on date T" is itself recorded.

## Migration checklist

1. **Stand up Lunaris alongside Zep.** Use `examples/quickstart-py`.
   No infra commitment beyond docker-compose.
2. **Map `session_id` → `Scope`.** One-line conversion. Validate
   that all your session IDs match `[A-Za-z0-9_\-.]{1,128}`. If they
   don't, replace `:`/`/` with `.` (most common compat work).
3. **Mirror writes.** Every `memory.add_memory(...)` is also
   dispatched to `mem.scoped(scope).ingest(...)`.
4. **Shadow reads.** Issue every `memory.search_memory(...)` to
   Lunaris in parallel. Diff the result sets in your eval harness.
5. **Cutover when the diff is acceptable.** Promote Lunaris to
   primary; keep Zep as fallback for ~1 release.
6. **Decommission Zep.** You're now running one Rust process
   (your agent) + Moon. No Python service to operate,
   no Neo4j to back up.

## When to stay on Zep

- You're not ready to operate a substrate (Moon) and
  prefer Zep Cloud's hosted plan.
- Your agent stack is pure Python and adding a Rust binary to your
  build pipeline is friction.
- You're already invested in Zep's MemGPT-style consolidation and
  the ACT-R Leiden approach is unfamiliar territory.
- You need recall latency ≤ 500 ms but not ≤ 25 ms — Zep is
  production-ready at that envelope and Lunaris's edge isn't free
  to operate.

## Known gaps vs Zep today

- **Hosted SaaS.** Lunaris does not (yet) offer a managed service.
  Self-host via Docker / Helm today; a managed service remains on the
  roadmap.
- **MemGPT-style salience.** Lunaris's consolidator implements
  ACT-R (Anderson 1996) base-level activation + Petrov 2006 O(1)
  incremental approximation + Leiden community detection. The
  numbers are different; the semantics are not strictly worse
  ("more recent + more frequent + more connected" → higher
  activation). If your evals depend on Zep's specific recency
  weighting, port the eval first.
- **OpenAI-default embedder.** Zep ships with an opinionated
  embedder; Lunaris is unopinionated — you choose llama.cpp
  (local 768d), Ollama, or a cloud API. v0.3 ships a Helm chart
  with a sensible default.

See `docs/MIGRATING-FROM-MEM0.md` for the parallel migration story
from Mem0. The two docs differ because Mem0 has no bi-temporal model
while Zep does — Mem0 migrations focus on the bi-temporal upgrade;
Zep migrations focus on the latency + substrate simplification.
