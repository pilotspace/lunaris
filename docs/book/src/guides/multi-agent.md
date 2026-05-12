# Multi-Agent & Scope

**Reach for this chapter when one Lunaris deployment serves more than one
agent or tenant.** `Scope` is the partition key: every ingest, recall, and
consolidation/verification operation is bound to a scope, and an agent cannot
read, write, or delete across scope boundaries. Multi-agent isolation is the
headline OSS positioning — Lunaris enforces it at the type level (RFC 0001),
not by convention.

## The `Scope` newtype

`Scope` (`lunaris_core::Scope`, `crates/lunaris-core/src/scope.rs`) is a thin
newtype around `SmolStr` — short identifiers stay inline, clones are O(1).
Two scopes compare equal iff their strings match byte-for-byte. **There is no
implicit "default" scope** — you construct one explicitly.

```rust
use lunaris::Scope;
let s = Scope::new("acme.agent-1")?;   // -> Ok(Scope)
assert_eq!(s.as_str(), "acme.agent-1");
```

### Alphabet — `[A-Za-z0-9_\-.]{1,128}`, no colons

`Scope::new` validates against `^[A-Za-z0-9_\-.]{1,128}$`. The hand-rolled
`Deserialize` impl re-runs the *same* validator on wire bytes — wire data is
never trusted (RFC 0001 §11). The colon was **removed in v0.2.1** so the Moon
KV format `lunaris:{scope}:{kind}:{ulid}` cannot byte-alias across scopes
(RFC 0001 §11.3, RC-2); Postgres enforces the same alphabet via a per-table
`<table>_scope_check` constraint. Use **dots** in identifiers: `acme.agent-42`,
not `acme:agent-42`.

> **Upgrading from v0.2.0?** Any scope/JWT-tenant string containing `:` now
> fails at the HTTP boundary with `invalid scope`. Rotate JWTs (`acme:agent-42`
> → `acme.agent-42`); on Postgres, rewrite affected rows
> (`UPDATE <table> SET scope = replace(scope, ':', '.')` for each primitive
> table) **before** running migration `20260512000007_scope_regex_tighten.sql`.
> Moon keys are immutable — re-ingest colon-keyed data under the rewritten
> scope. Recipe: RFC 0001 §11.4.

`Scope::dev()` is `#[doc(hidden)] pub` — a test/migration crutch only. Any
`Scope::dev()` call site in production code is a carry-over, not a pattern.
(`Lunaris::forget` still uses it internally in v0.2.x — see [Forgetting](./forget.md).)

## Binding an engine to a scope — `ScopedLunaris<'a>`

```rust
use lunaris::{EpisodeBuilder, Lunaris, Query, Scope, Vector};

let lunaris = Lunaris::open("postgres://lunaris@localhost/lunaris").await?;
let scoped  = lunaris.scoped(Scope::new("acme.agent-1")?);   // ScopedLunaris<'_>

let lsn  = scoped.ingest(EpisodeBuilder::new("notes.md", "Alice met Bob.")).await?;
let hits = scoped.dsl()
    .with_root(Vector::new("chunks", 30).top(5))
    .execute(Query::text("Alice"))
    .await?;
```

`ScopedLunaris::ingest` takes an `EpisodeBuilder` (scope-less payload) and
stamps its own scope — callers cannot override the bound scope mid-call. That
is the compile-time guard: "ingest into agent A, retrieve from agent B" can't
type-check. `ScopedLunaris::recall(query)` is the one-shot form; `.dsl()`
returns a `RetrievalBuilder` pre-seeded with the scope for chained queries.
`.scope()` returns the bound `&Scope`.

Under the hood `&Scope` is threaded through every partitioned `StoragePort`
method (`atomic_write`, `vector_search`, `graph_traverse`, `scan_range`,
`read_as_of`, `publish`, `subscribe`, `queue_depth`) and `KeywordPort::keyword_search`
— Postgres partitions via RLS, Moon via a per-scope keyspace prefix
`lunaris:{scope}:` and per-scope FT/GRAPH/MQ resources.

## Per-scope worker supervision

The consolidator and verifier slow paths run a `JoinSet<()>` of one task per
active scope (`ConsolidateSupervisor` / `VerifySupervisor`,
`crates/lunaris-consolidate/src/supervisor.rs`,
`crates/lunaris-verify/src/supervisor.rs`). A new scope is registered from a
heartbeat the HTTP server sends on every authenticated request. **Failure
isolation:** a panic in scope A's worker doesn't stall scope B — the supervisor
restarts only the failed scope's task.

| Variable | Default | Controls |
|---|---|---|
| `LUNARIS_SCOPE_CONCURRENCY` | `8` | Max concurrent event-batch tasks **per scope** (semaphore — a hot scope can't saturate the embedder GPU) |
| `LUNARIS_SCOPE_IDLE_TIMEOUT_MS` | `1800000` (30 min) | Idle-scope worker eviction — sheds dormant tasks so file descriptors don't leak at high scope counts |
| `LUNARIS_WORKER_DRAIN_MS` | `5000` (5 s) | Graceful drain window when a scope worker shuts down |

**Operational ceiling:** N scopes ⇒ N Moon FT indices and N MQ topics. Moon's
soft limit is ~512 FT indices per node before recall p99 degrades — surfaced
as `StorageCapabilities::max_scopes_recommended`. Above that, multi-tenant
pooling is a future RFC. Postgres has no such per-scope resource cost.

See [Configuration → Supervision / worker pool](../reference/configuration.md#supervision--worker-pool).

## The HTTP multi-agent contract

Over `lunaris-server`, the scope comes from the JWT `tenant` claim in the
bearer-token map — it is the **only** source of truth for the partition scope.
Route handlers ignore any `scope` / `tenant` field on the request body; all
public request DTOs carry `#[serde(deny_unknown_fields)]`.

```json
{
  "my-bearer-token": { "tenant": "agent.helios", "scopes": ["ingest", "recall", "forget"] }
}
```

`tenant` must match `^[A-Za-z0-9_\-.]{1,128}$` — anything else is `401` on
every request that uses the token. A token lacking the route's required scope
(the `scopes` array) is `403`. The five executable UAT scenarios below map 1:1
to `crates/lunaris-server/tests/multi_agent_uat.rs`; an external consumer's CI
gate is met when all five pass.

### UAT-1 — cross-scope ingest + recall isolation

Two tokens, tenants `agent.alpha` and `agent.beta`.

```bash
# ingest under alpha
curl -X POST http://localhost:8080/v1/ingest -H "Authorization: Bearer tok-alpha" \
  -H "Content-Type: application/json" \
  -d '{"source":"agent-alpha:notes","content":"Alice met Bob today"}'
# -> 200 {"lsn":{"wall_ms":...,"counter":1},"queue_lag_warn":false}

# ingest under beta
curl -X POST http://localhost:8080/v1/ingest -H "Authorization: Bearer tok-beta" \
  -H "Content-Type: application/json" \
  -d '{"source":"agent-beta:reports","content":"Quarterly revenue grew 12%"}'

# recall "Alice" as alpha -> 200, non-empty, first hit text contains "Alice"
curl -X POST http://localhost:8080/v1/recall -H "Authorization: Bearer tok-alpha" \
  -H "Content-Type: application/json" -d '{"query":"Alice","k":5}'

# recall "Alice" as beta -> 200, EMPTY array [] — no cross-scope leak
curl -X POST http://localhost:8080/v1/recall -H "Authorization: Bearer tok-beta" \
  -H "Content-Type: application/json" -d '{"query":"Alice","k":5}'

# recall "revenue" as beta -> 200, non-empty (its own data)
curl -X POST http://localhost:8080/v1/recall -H "Authorization: Bearer tok-beta" \
  -H "Content-Type: application/json" -d '{"query":"revenue","k":5}'
```

### UAT-2 — malformed scope → 401

A token whose `tenant` is empty, longer than 128 chars, or contains a
character outside `[A-Za-z0-9_\-.]` (`%`, space, `\`, **`:`**) is rejected
*before any handler runs*:

```json
{"error":"unauthorized","message":"token tenant is not a valid scope identifier"}
```

### UAT-3 — request body cannot override scope

`IngestBody` is `#[serde(deny_unknown_fields)]`, so a `"scope"` or `"tenant"`
key in the body is `422 Unprocessable Entity` before the handler runs — and
zero episodes are written for the caller's scope (no partial writes).

```bash
curl -X POST http://localhost:8080/v1/ingest -H "Authorization: Bearer tok-alpha" \
  -H "Content-Type: application/json" \
  -d '{"source":"evil","content":"x","scope":"victim-scope"}'
# -> 422 {"error":"unprocessable_entity","message":"... unknown field `scope` ..."}
```

### UAT-4 — forget honors scope

A cross-scope forget finds zero rows in the caller's partition and has no
effect on anyone else's data — `rows_written == 0`, `rows_deleted == 0`,
`preview` reflects the request. (Note the v0.2.x [forget caveat](./forget.md):
`Lunaris::forget` is `_dev_`-scoped internally; full `ScopedLunaris::forget`
with a `403`/`404` cross-scope contract lands in v0.3 — consumers should be
ready for both shapes.)

### UAT-5 — concurrent multi-agent traffic smoke

10 agents (tenants `agent.0` … `agent.9`), each 3 ingest + 3 recall calls
concurrently (60 HTTP calls). All `200 OK`; no agent sees another's data in
its recall results.

## Gotchas

- **No hierarchical scopes** in v0.2 — flat strings only. `org/team/agent`
  tree semantics are a future RFC.
- **Cross-scope graph references are disallowed by construction** —
  `Relation.src` / `Relation.dst` must resolve within the same scope.
- **`Scope` is an identifier, not a permission system** — AuthZ stays in the
  `lunaris-server` middleware (the token map's `scopes` array).
- **v0.1 → v0.2 has no on-the-wire compatibility.** Migration is SQL-driven
  on Postgres — `migrations/20260510000005_scope_partitioning.sql` backfills
  `scope = '_legacy'` for pre-scope rows (`_legacy` is the reserved fallback
  literal), and `metadata.tenant` is no longer silently honored as a tenant
  key. Full recipe — including the v0.2.0 → v0.2.1 colon-removal step
  (`migrations/20260512000007_scope_regex_tighten.sql`) — in
  `docs/migration/0.1-to-0.2.md`.

## See also

- [Ingesting Observations](./ingest.md) and [The Retrieval DSL](./retrieval-dsl.md)
  — the scoped surface in detail.
- [Forgetting](./forget.md) — why `forget` is `_dev_`-only in v0.2.x.
- [Configuration Reference](../reference/configuration.md) — the bearer-token
  map format and supervision env vars.
- [MemoryProtocol 0.1](../protocol/memoryprotocol-0.1.md) — the full HTTP/SSE
  wire spec.
