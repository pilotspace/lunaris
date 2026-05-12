# Choosing a Backend (Moon vs Postgres)

**Lunaris ships two `StoragePort` implementations: Moon (a Redis-compatible
substrate, the performance default) and Postgres (pgvector + Apache AGE +
pgmq, the portability proof). Both pass the same conformance suite — pick by
operational fit, not feature parity.** `Lunaris::open(url)` dispatches on the
URL scheme: `moon://host:port` or `postgres://user:pass@host/db`.

## Feature parity at a glance

| | Moon (`lunaris-storage-moon`) | Postgres (`lunaris-storage-postgres`) |
|---|---|---|
| Vector search | Native `FT.SEARCH` (HNSW) | `pgvector` |
| BM25 keyword | Native `FT.SEARCH` inverted index | `pgvector` + Postgres FTS |
| RRF fusion | **Native** (in-substrate) | **Client-side** (Lunaris fuses in process) |
| Graph traversal | Native `GRAPH.QUERY` | Apache AGE Cypher |
| Pipeline queue | Native Streams | `pgmq` |
| Embedding dim (adapter) | **Embedder-sized** — the Moon adapter creates its vector index at the configured embedder's dimension (default **768-d**, EmbeddingGemma-300M; `Lunaris::open` passes `embedder.dim()` through, and `MoonStorage::connect_with_dim` lets you set it directly). No upper cap. | up to ~**1536-d** (pgvector practical ceiling) |
| Bi-temporal `as_of` | Native bi-temporal index | `tstzrange &&` |
| Tenant isolation | Per-scope keyspace prefix `lunaris:{scope}:` + per-scope FT/GRAPH/MQ | Postgres **RLS** (`SET LOCAL lunaris.scope`) |
| Scope soft cap | ~512 scopes/node (`max_scopes_recommended`) | n/a (RLS scales with rows) |
| Recovery | AOF + base-RDB replay — see [Durability](./durability.md) | WAL + fsync (Postgres-managed) |

The `StorageCapabilities` report a backend returns drives capability-gated
behaviour (graph mode, native vs client RRF, queue mode); the AS_OF parity
test (`lunaris_conformance::storage::as_of_parity::run`) asserts the two
backends return identical hits + ordering for the same input.

> **About the embedding dimension.** Moon (the substrate) has *no* hard
> vector-dimension limit — `FT.CREATE` only requires `DIM > 0`. The Moon
> adapter creates its `chunks` (and `entities` / `facts` / `communities`) FT
> indices at the **configured embedder's dimension**: `Lunaris::open(url)`
> reads `embedder.dim()` and calls `MoonStorage::connect_with_dim(url, dim)`,
> so a 1536-d embedder (OpenAI `text-embedding-3`) works against Moon out of
> the box. The default is **768-d** (EmbeddingGemma-300M); `max_vector_dim` in
> `StorageCapabilities` then reports whatever dimension the index was actually
> created at. Operator footgun: Moon's `FT.CREATE` is idempotent and does
> **not** resize an existing index — if a Moon instance already holds a 768-d
> `chunks` index and you reopen with a wider embedder, the old index stays and
> the mismatch only shows up on the first vector write; drop the stale index
> (`FT.DROPINDEX <name>`) first. And it remains a latency trade-off (wider
> vectors = more bytes/vector and more distance-compute per query), not a
> capability boundary.

## When to pick which

**Pick Moon when:**

- Recall latency is a hard contract — Moon is the path the sub-25 ms-p50 moat
  is measured on.
- You already run (or are willing to run) Moon as infra.
- The Moon adapter sizes its FT vector index to your embedder — the default
  768-d (EmbeddingGemma-300M) is the common case, but a wider embedder works
  on Moon too (`Lunaris::open` passes `embedder.dim()` through). See the note
  above for the "existing index won't auto-resize" footgun.
- You don't need more than ~512 scopes per node.

**Pick Postgres when:**

- You already operate Postgres and want zero new infra.
- You want pgvector's SQL-queryable storage and up to ~1536-d embeddings —
  e.g. OpenAI `text-embedding-3`. (Moon now also sizes its index to wider
  embedders, so this is a "SQL access + ecosystem" choice, not a hard
  dimension boundary.)
- You want the database boundary itself (RLS) enforcing tenant isolation, or
  you want ad-hoc SQL access to the stored primitives.
- The extra ~hundreds of ms vs Moon on the hot path is acceptable.

Either choice is correct; the conformance suite makes the surface identical.

## Postgres setup (pgvector + AGE + pgmq)

The backend expects three extensions available in the target database:

- **`pgvector`** — vector column type + HNSW index for `vector_search`.
- **Apache AGE** — Cypher graph queries for `graph_traverse`. Each session
  bootstraps with `LOAD 'age'` + `SET search_path = ag_catalog, "$user",
  public` (`crates/lunaris-storage-postgres/src/pool.rs`).
- **`pgmq`** — message queue for the consolidate / verify pipeline queues.

Schema is sqlx-managed (`crates/lunaris-storage-postgres/migrations/`):

```bash
sqlx migrate run --source crates/lunaris-storage-postgres/migrations \
                 --database-url $LUNARIS_PG_URL
```

`PgClient::connect(url)` runs migrations on connect; `connect_no_migrate(url)`
skips DDL — use the latter for the non-privileged app role (below) and let a
privileged connection apply migrations first. Pool size is fixed at
`max_connections(8)` in code.

### The `NOSUPERUSER NOBYPASSRLS` role recipe

**This is not optional for multi-tenant production.** Postgres superusers
(`rolsuper=t`) and roles with `BYPASSRLS` skip every RLS policy regardless of
`FORCE ROW LEVEL SECURITY` — connecting as such silently disables scope
isolation. Create a dedicated role (from `docs/migration/0.1-to-0.2.md` §6.2):

```sql
CREATE ROLE lunaris_app WITH LOGIN PASSWORD '…' NOSUPERUSER NOBYPASSRLS;
GRANT USAGE ON SCHEMA public TO lunaris_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO lunaris_app;
GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO lunaris_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO lunaris_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT USAGE ON SEQUENCES TO lunaris_app;
```

Then connect via `postgres://lunaris_app@host/lunaris`. If you observe
cross-scope reads in a test environment, **check the connection role first** —
`crates/lunaris-storage-postgres/tests/scope_isolation.rs` is a false-pass
under a superuser.

### RLS notes (the invariants)

- **Every read path opens a read tx and runs `SELECT set_config('lunaris.scope',
  $1, true)` before the body** — mirror the `vector.rs::vector_search`
  pattern. The v0.2 review found `keyword_search` skipping this; BM25 then
  silently returned zero hits under the app role (fixed in v0.2.1, RC-A).
  Tests that exercise a read path MUST run under the app role, not the owner —
  owner/superuser tests pass by accident.
- **Every `tenant_isolation` policy declares both `USING` and `WITH CHECK`.**
  `USING`-only is read-tight on SELECT/UPDATE but leaves INSERT
  scope-unchecked at the database boundary (RC-3; migration
  `20260511000006_rls_with_check.sql`).
- The `scope` column is `TEXT NOT NULL` on every primitive table; the
  per-table `<table>_scope_check` constraint enforces the same
  `[A-Za-z0-9_\-.]{1,128}` alphabet `Scope::new` uses (migration 7) — so a
  scope string can never byte-alias across tenants.

## Moon setup

Moon needs no schema migration — per-scope keyspaces, FT indices, GRAPH keys,
and MQ topics are created lazily on the first `atomic_write` per scope (so the
first write per new scope is slightly slower; subsequent writes hit the warm
index). For durable operation see [Durability & Recovery](./durability.md) —
the short version is: enable AOF (`--appendonly yes --appendfsync always`) and
ensure a base RDB exists (`BGREWRITEAOF`, **not** `BGSAVE`) before you trust
the data.

## See also

- [Durability & Recovery](./durability.md)
- [Configuration Reference §4 — Storage URL scheme](../reference/configuration.md#4-storage-url-scheme)
- [Multi-Agent & Scope](../guides/multi-agent.md)
- [Conformance](../protocol/conformance.md) — the parity guarantees in code
