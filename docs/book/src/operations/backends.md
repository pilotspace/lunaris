# The Storage Backend (Moon)

**As of 0.7.0 Lunaris ships exactly one `StoragePort` implementation: Moon,
the Redis-compatible substrate.** `Lunaris::open(url)` accepts one scheme,
`moon://host:port`; every retired spelling (`postgres://`, `postgresql://`,
`memory://`, `sqlite:///path`) was removed in 0.7.0 and returns
`UnsupportedScheme` carrying the migration link rather than half-working.

If you are on 0.6.x with a Postgres or SQLite store, **migrate before you bump
the pin** — the exit ramp is `lunaris-migrate`, built from the v0.6.2 tag. See
[0.6 → 0.7](https://github.com/pilotspace/lunaris/blob/main/docs/migration/0.6-to-0.7.md).

## What Moon provides

| Capability | Implementation |
|---|---|
| Vector search | Native `FT.SEARCH` (HNSW) |
| BM25 keyword | Native `FT.SEARCH` inverted index |
| RRF fusion | **Native, in-substrate** — a (Vector + Keyword) pair on one index is a single round trip |
| Graph traversal | Native `GRAPH.QUERY` (Cypher) |
| Pipeline queue | Native Streams |
| Embedding dim | **Embedder-sized** — the adapter creates its vector index at `embedder.dim()` (default **768-d**). No upper cap. |
| Bi-temporal `as_of` | Native, on the **search and graph** lanes (`FT.SEARCH AS_OF`, `GRAPH.QUERY VALID_AT`) |
| Historical KV read | **Not supported** — see STORE-07 below |
| Tenant isolation | Per-scope keyspace prefix `lunaris:{scope}:` + per-scope FT / GRAPH / MQ |
| Scope soft cap | ~512 scopes/node (`max_scopes_recommended`) |
| Recovery | AOF + base-RDB replay — see [Durability](./durability.md) |

The `StorageCapabilities` report the backend returns still drives
capability-gated behaviour (graph mode, native vs client RRF, queue mode). It
is no longer a *portability* mechanism — with one backend it is how the engine
learns what this Moon build supports, and it is what the STORE-07 refusal below
is derived from.

### STORE-07 — no historical KV reads

Moon has no per-key version chain, so `read_as_of` cannot walk one.
`supports_historical_kv_reads()` returns `false` and the call **refuses**
rather than silently answering with today's value — a wrong answer to "what did
the agent know at T?" is worse than a named error. Through 0.6.x this was the
one capability Postgres/SQLite had that Moon did not; with those backends gone
it is a flat limitation of 0.7.0, pinned by
`lunaris_conformance::storage::read_as_of` and the `run_as_of_moon_gap` test.

Time-travel over **search** and **graph** results is unaffected and native.

### About the embedding dimension

Moon has no hard vector-dimension limit — `FT.CREATE` only requires `DIM > 0`.
The adapter creates its `chunks` (and `entities` / `facts` / `communities`) FT
indices at the **configured embedder's dimension**: `Lunaris::open(url)` reads
`embedder.dim()` and calls `MoonStorage::connect_with_dim(url, dim)`, so a
1536-d embedder (OpenAI `text-embedding-3`) works out of the box. The default
is **768-d** (granite-embedding-311m-multilingual-r2); `max_vector_dim` in
`StorageCapabilities` reports whatever dimension the index was actually created
at.

> **Operator footgun.** `FT.CREATE` is idempotent and does **not** resize an
> existing index. If a Moon instance already holds a 768-d `chunks` index and
> you reopen with a wider embedder, the old index stays and the mismatch only
> surfaces on the first vector write — drop the stale index
> (`FT.DROPINDEX <name>`) first. Wider vectors remain a latency trade-off (more
> bytes/vector, more distance-compute per query), not a capability boundary.

## Moon setup

```bash
docker run -d --name lunaris-moon -p 6380:6379 \
  ghcr.io/pilotspace/moon:0.8.5 \
  --shards 1 --protected-mode no --appendonly yes
```

Two flags are load-bearing:

- **`--shards 1` is mandatory.** A Lunaris ingest is one MULTI/EXEC
  transaction, and a sharded Moon rejects cross-shard writes — every ingest
  fails. The image defaults to `--shards 0` (auto-detect), so the flag has to
  be passed explicitly.
- **`--appendonly yes`** is what makes the store survive a restart.

There is **no schema migration and no role bootstrap**. Per-scope keyspaces, FT
indices, GRAPH keys, and MQ topics are created lazily on the first
`atomic_write` per scope, so the first write for a new scope is slightly slower
and subsequent writes hit the warm index.

Before you trust the data on disk, read [Durability &
Recovery](./durability.md) — the short version is: enable AOF
(`--appendonly yes --appendfsync always`) and ensure a base RDB exists via
`BGREWRITEAOF` (**not** `BGSAVE`).

Full production setup — memory limits, backups, health probes, systemd/launchd
units — is in
[Running an external Moon](https://github.com/pilotspace/lunaris/blob/main/docs/operations/external-moon.md).

## See also

- [Durability & Recovery](./durability.md)
- [Configuration Reference §4 — Storage URL scheme](../reference/configuration.md#4-storage-url-scheme)
- [Multi-Agent & Scope](../guides/multi-agent.md)
- [Conformance](../protocol/conformance.md)
