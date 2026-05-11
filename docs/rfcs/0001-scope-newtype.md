# RFC 0001 — `Scope` newtype and `ScopedLunaris<'a>` typestate

| Field        | Value                                          |
|--------------|------------------------------------------------|
| Status       | **Implemented** (v0.2.0, 2026-05-11)           |
| Author       | Tin Dang                                       |
| Created      | 2026-05-10                                     |
| Implemented  | 2026-05-11 (Wave 4 close-out)                  |
| Target       | Lunaris **v0.2.0** OSS                         |
| Supersedes   | —                                              |
| Related      | `tmp/lunaris-ship-to-product-v2.md`, `tmp/lunaris-multi-agent-flow.md`, `tmp/xmem-grounded-findings-and-pickups.md` |

---

## 1. Summary

Introduce a first-class `Scope` newtype and a `ScopedLunaris<'a>` typestate
wrapper so every ingest, retrieval, consolidation, and queue operation is
**partitioned by scope at compile time**. Scope is the primary key under which
multi-agent / multi-tenant isolation is enforced — by Postgres RLS in the
Postgres backend, and by per-scope shard keyspaces in the Moon backend.

The change is a **breaking API change** at the v0.1 → v0.2 boundary. We ship
it now, before external adopters are locked in, instead of later when an
out-of-band tenant key would become a credibility wound (Mem0 / Zep / Cognee
all carry that scar).

---

## 2. Motivation

### 2.1 Today (v0.1)

Verified against `crates/lunaris-core/src/primitives.rs:16-24`,
`crates/lunaris/src/ingest.rs:72`, and
`crates/lunaris-server/src/middleware/auth.rs:30`:

- `Episode` carries `id`, `source`, `content`, `t_ref`, `bt`, `metadata` — **no
  agent_id, no tenant, no scope**.
- `Lunaris::ingest(&self, episode: Episode) -> Result<Lsn, _>` has **no scope
  parameter**.
- `StoragePort::atomic_write(&self, ops: &[WriteOp])` writes into a **single
  global keyspace**.
- `lunaris-server` already extracts `AuthClaims { tenant: String, scopes:
  Vec<String> }` and a `TenantKey` rate-limit extractor — but **the tenant
  string is dropped on the floor** before reaching the engine; it survives only
  as a `metadata["tenant"]` JSON field on the episode (`crates/lunaris-server/src/routes/ingest.rs:41-76`).
- Cross-scope isolation today is **convention only**: a recipe author who
  forgets to filter by `metadata.tenant` leaks data across agents.

### 2.2 Why v0.2 instead of later

1. **Multi-agent is the headline OSS positioning.** Every public LangGraph
   memory layer (Mem0, Zep, Cognee, XMem) advertises agent isolation. Lunaris
   currently cannot — the README's "agent memory engine" claim is unsupported
   without scope.
2. **Breaking-change debt compounds.** Adding `&Scope` later means a 0.1 → 0.3
   migration path, every SDK (Py / TS) re-cut, every recipe rewritten. Every
   week we delay raises switching cost for adopters.
3. **The advisor flagged Scope-as-string as the highest-risk shortcut.** A
   typed newtype makes "ingest into agent A, retrieve from agent B" a compile
   error. A `String` makes it a silent data leak.
4. **Postgres RLS needs a column to key on.** Without `scope` as a first-class
   column on every primitive table, RLS policies cannot be written.

### 2.3 What we are NOT doing in this RFC

- Cross-scope graph references — disallowed by construction in v0.2.
  `Relation.src` / `Relation.dst` MUST resolve within the same scope. (Future
  RFC if needed.)
- Per-scope ACL / role grants — `Scope` is an identifier, not a permission
  system. AuthZ remains in `lunaris-server` middleware.
- Hierarchical scopes (`org/team/agent`) — flat `SmolStr` for v0.2; tree
  semantics deferred.

---

## 3. Design

### 3.1 The `Scope` newtype (`lunaris-core::primitives`)

```rust
use smol_str::SmolStr;

/// A partition key for multi-agent / multi-tenant isolation.
///
/// `Scope` is a thin newtype around `SmolStr` (inline up to 23 bytes — most
/// scope identifiers fit). Two scopes compare equal iff their string forms
/// match byte-for-byte. There is **no implicit fallback to a "default"
/// scope** — a `Scope` must be constructed explicitly.
///
/// # Validation
///
/// The string must match `^[A-Za-z0-9_\-:.]{1,128}$`. This is enforced by
/// `Scope::new`; the unchecked constructor is `pub(crate)` and only used by
/// trusted internal call sites (deserialization of validated wire data).
///
/// # Examples
///
/// ```
/// use lunaris_core::Scope;
/// let s = Scope::new("acme:agent-42").unwrap();
/// assert_eq!(s.as_str(), "acme:agent-42");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Scope(SmolStr);

impl Scope {
    pub fn new(s: impl AsRef<str>) -> Result<Self, ScopeError> { /* validate + construct */ }
    pub fn as_str(&self) -> &str { self.0.as_str() }
    pub fn as_bytes(&self) -> &[u8] { self.0.as_bytes() }
}

#[derive(Debug, thiserror::Error)]
pub enum ScopeError {
    #[error("scope must be 1..=128 chars of [A-Za-z0-9_\\-:.]; got {0:?}")]
    Invalid(String),
}
```

**Why `SmolStr`:** every primitive write-op path tags the row with the scope.
`SmolStr` keeps short scopes inline (no heap alloc) and is `Clone`-cheap.

**Why not `Cow<'static, str>`:** lifetime in the type would infect every
trait method, and the typical scope is short enough that `SmolStr`
pays for itself in zero-alloc clones.

### 3.2 `Episode` and primitives gain a `scope` field

```rust
pub struct Episode {
    pub id: Ulid,
    pub scope: Scope,                     // NEW — required at construction
    pub source: String,
    pub content: String,
    pub t_ref: Option<DateTime<Utc>>,
    pub bt: BiTemporal,
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

// Constructor signature changes:
impl Episode {
    pub fn new(
        scope: Scope,                     // NEW
        source: impl Into<String>,
        content: impl Into<String>,
        clock: &HlcClock,
    ) -> Self { /* ... */ }
}
```

`Chunk`, `Entity`, `Relation`, `Claim`, `Source` likewise gain a `scope:
Scope` field. Every persisted primitive carries its scope — RLS policies and
Moon shard keys both consume it.

### 3.3 `ScopedLunaris<'a>` typestate

```rust
pub struct Lunaris { /* unchanged engine */ }

impl Lunaris {
    /// Bind this engine to a scope. Returns a wrapper whose every method
    /// inherits the scope without re-passing it.
    pub fn scoped<'a>(&'a self, scope: Scope) -> ScopedLunaris<'a> {
        ScopedLunaris { engine: self, scope }
    }
}

pub struct ScopedLunaris<'a> {
    engine: &'a Lunaris,
    scope: Scope,
}

impl<'a> ScopedLunaris<'a> {
    pub async fn ingest(&self, ep: EpisodeBuilder) -> Result<Lsn, LunarisError> { /* ... */ }
    pub async fn retrieve(&self, q: RetrievalQuery) -> Result<Vec<Hit>, LunarisError> { /* ... */ }
    pub fn dsl(&self) -> ScopedRetrievalBuilder<'_, 'a> { /* ... */ }
    pub fn scope(&self) -> &Scope { &self.scope }
}

// Bare `Lunaris::ingest(Episode)` becomes deprecated then removed (see §6).
```

**Mistake guard:** `ScopedLunaris<'a>::ingest` takes an `EpisodeBuilder`
(a no-scope payload) — the wrapper writes its own scope onto the episode
before handing it to the engine. Callers cannot *override* the bound scope
mid-call. This is the type-system enforcement that makes cross-agent leaks a
compile error.

### 3.4 `StoragePort` and `KeywordPort` signature changes

The `StoragePort` trait acquires an explicit `&Scope` argument on every method
that needs to partition (everything except pure metadata operations). The arrow
points inward — the trait stays in `lunaris-core`, implementations land in
`lunaris-storage-postgres` and `lunaris-storage-moon`.

**Amendment (Wave 2.5A, 2026-05-11):** `KeywordPort::keyword_search` was
overlooked in the Wave 0 type freeze — `&Scope` was added to `StoragePort`'s
8 methods but not to the BM25 extension trait. Wave 2.5A closes the gap:
`keyword_search` gains `scope: &Scope` as its first argument after `&self`.
Moon routes to the per-scope FT index; Postgres accepts the parameter for API
parity (partitioning is enforced via RLS at the connection level, Wave 1B).

```rust
#[async_trait]
pub trait StoragePort: Send + Sync + 'static {
    async fn atomic_write(
        &self,
        scope: &Scope,                    // NEW
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError>;

    async fn vector_search(
        &self,
        scope: &Scope,                    // NEW
        index: &str,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
        rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError>;

    async fn graph_traverse(
        &self,
        scope: &Scope,                    // NEW
        query: &CypherQuery,
        as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError>;

    async fn scan_range(
        &self,
        scope: &Scope,                    // NEW
        prefix: &[u8],
        as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError>;

    async fn read_as_of(
        &self,
        scope: &Scope,                    // NEW
        key: &[u8],
        as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError>;

    async fn publish(
        &self,
        scope: &Scope,                    // NEW
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError>;

    async fn subscribe(
        &self,
        scope: &Scope,                    // NEW
        group: &str,
        topic: &str,
        partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError>;

    async fn queue_depth(
        &self,
        scope: &Scope,                    // NEW
        topic: &str,
        partition: u16,
    ) -> Result<u64, StorageError>;

    fn capabilities(&self) -> StorageCapabilities;
}
```

`WriteOp` variants do NOT carry a scope — the trait method's `scope`
parameter is authoritative for the whole batch. (Rationale: a single
`atomic_write` is by definition one scope; cross-scope atomicity is
explicitly out of scope for v0.2.)

### 3.5 Postgres backend (`lunaris-storage-postgres`)

- Every primitive table gains a `scope TEXT NOT NULL` column with a
  `CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$')` constraint and an index on
  `(scope, id)`.
- Migration `0xx_scope.sql` (Alembic-equivalent — sqlx migrations) backfills
  existing rows with `scope = '_legacy'` (a reserved literal — see §6).
- **Row-Level Security** is enabled per table:
  ```sql
  ALTER TABLE episodes ENABLE ROW LEVEL SECURITY;
  CREATE POLICY tenant_isolation ON episodes
    USING (scope = current_setting('lunaris.scope'));
  ```
  The `MoonStorage` Postgres adapter sets `SET LOCAL lunaris.scope = $1`
  inside every transaction before issuing primitive ops.
- Cross-scope leak test: a CI test inserts in `scope=A`, sets
  `lunaris.scope=B`, and asserts queries return zero rows. **Mandatory gate.**

### 3.6 Moon backend — one-shard-per-scope (`lunaris-storage-moon`)

User-elected: Moon partitioning ships in v0.2 (more aggressive than the
advisor's "Postgres-first, Moon in 0.2.5" recommendation).

- **Keyspace prefix per scope:** every Moon key is prefixed
  `lunaris:{scope}:` — replacing today's `lunaris:`. `keyspace::scope_prefix(&scope)`
  is the single source of truth and is invoked by every command in
  `atomic.rs`, `vector.rs`, `graph.rs`, `kv.rs`, `keyword.rs`, `queue.rs`.
- **FT index per scope:** `FT.CREATE lunaris_{scope}_chunk_idx`. The router
  in `MoonStorage::vector_search` consults a per-scope index map.
- **MQ topic per scope:** `MQ.PUSH lunaris:{scope}:consolidate`. The
  consolidate / verify workers consume one topic per scope, so a hot scope
  cannot starve a cold one (see §3.7).
- **GRAPH key per scope:** `GRAPH.QUERY lunaris_{scope}_graph`.
- **Moon workspace alternative** (`?ws=<scope>`): considered, **not chosen**
  for v0.2 — keyspace prefix is uniform across all command types and works on
  current Moon without coordinating with the Moon team for workspace ACLs.
  Workspace mapping is a future RFC if scope counts exceed Moon FT index
  limits.

**Operational risk:** N scopes ⇒ N FT indices and N MQ topics. The
`StorageCapabilities` struct gains `max_scopes_recommended` reflecting Moon's
soft limit (currently ~512 FT indices per Moon node before recall p99
degrades — see Moon docs). Above that, multi-tenant pooling becomes a future
RFC.

### 3.7 Consolidator and Verifier — per-scope queues

`run_consolidate_worker` and `run_verify_worker` today consume a single
global topic. They become per-scope:

- The worker spawns a `JoinSet<()>` of one task per active scope.
- A new scope is detected via a heartbeat from `lunaris-server` (which sees
  every authenticated request) and registered with the worker pool.
- Per-scope concurrency cap (`Semaphore::new(N)`) prevents a hot scope from
  saturating the embedder GPU.

Failure isolation: a panic in the consolidator for scope A does not affect
scope B — `JoinSet` returns the error, the supervisor restarts only the
failed scope's task.

### 3.8 `lunaris-server` — `AuthClaims.tenant → Scope`

```rust
// middleware/auth.rs
pub struct AuthClaims { pub scope: Scope, /* … */ }   // was tenant: String

// routes/ingest.rs
async fn ingest_handler(
    Extension(claims): Extension<AuthClaims>,
    State(engine): State<Arc<Lunaris>>,
    Json(body): Json<IngestBody>,
) -> Result<Json<IngestResponse>, ApiError> {
    let scoped = engine.scoped(claims.scope.clone());
    let lsn = scoped.ingest(body.into_episode_builder()).await?;
    Ok(Json(IngestResponse { lsn }))
}
```

- The `tenant` JWT claim is parsed into `Scope::new(...)?` at the auth
  middleware boundary (fail-closed: invalid scope ⇒ 401).
- `metadata["tenant"]` is removed from `IngestBody` — it cannot override
  the JWT-bound scope. (Removing this is what closes today's silent-leak
  surface.)

---

## 4. Implementation Plan — Wave-Gated DAG

### Wave 0 — Sequential, owner (this RFC + stub commit)

```
docs/rfcs/0001-scope-newtype.md            (this document)
└─ Stub commit on branch v0.2/scope:
   ├─ Scope newtype + ScopeError in lunaris-core
   ├─ ScopedLunaris<'a> wrapper in crates/lunaris (todo!() bodies)
   ├─ StoragePort signatures gain &Scope (impls return todo!())
   ├─ Episode/Chunk/Entity/Relation/Claim/Source carry pub scope: Scope
   └─ Workspace COMPILES; cargo test produces a known list of `todo!()` panics
```

**This is the type freeze.** From this commit forward the public API is
contract. Wave 1 subagents implement against frozen signatures.

### Wave 1 — Parallel, 5 sonnet subagents (worktrees)

```
A. lunaris-core              — Scope validation, primitives migration, ScopeError tests
B. lunaris-storage-postgres  — schema migration, RLS policies, cross-scope leak test
C. lunaris-storage-moon      — keyspace prefix, FT/GRAPH/MQ per-scope routing  ⬅ NEW
D. lunaris-ingest + Lunaris  — ScopedLunaris<'a>, EpisodeBuilder, recipe migration
E. lunaris-server            — AuthClaims.tenant → Scope, middleware, routes
```

Each subagent works in `isolation: "worktree"`. No subagent edits another's
crate. The verifier subagent + owner gate them all before Wave 2.

### Wave 1 verifier gates (mandatory)

```
□ cargo build --workspace --all-features
□ cargo clippy --workspace -- -D warnings
□ cargo test --workspace (excluding lunaris-py + lunaris-ts cdylibs)
□ cargo fmt --check
□ INGEST-04 grep: exactly ONE atomic_write call per ingest path (unchanged)
□ No raw String IDs in StoragePort method signatures
□ Postgres RLS leak test: ingest into scope A, read as scope B ⇒ zero rows
□ Moon partition leak test: ingest into scope A, FT.SEARCH scope B ⇒ zero hits ⬅ NEW
□ Helios live UAT against ScopedLunaris ⇒ green
```

### Wave 2 — Sequential, owner

```
□ Review ScopedLunaris ergonomics (DSL chaining, async borrow propagation)
□ Public-api diff: cargo public-api-crates against v0.1.2 baseline
□ Migration guide stub: docs/migration/0.1-to-0.2.md
```

### Wave 3 — Parallel, 3 sonnet subagents

```
F. consolidator + verifier   — per-scope JoinSet, semaphore, supervisor
G. lunaris-py + lunaris-ts   — SDK regen, scope kwargs in DSL, type stubs
H. Helios consumer migration — full UAT against scoped HTTP server
```

### Wave 3 verifier gates

```
□ All Wave 1 gates re-pass
□ maturin develop ⇒ pytest passes (lunaris-py)
□ napi build ⇒ vitest passes (lunaris-ts)
□ Helios golden recipes pass scoped + cross-scope leak suite
□ Per-scope queue isolation test: scope A panic ⇏ scope B stall
```

### Wave 4 — Sequential, owner

```
□ docs/migration/0.1-to-0.2.md final
□ docs/multi-agent.md (new — hooks, recipes, RLS deployment)
□ CHANGELOG entry with breaking-change call-out
□ RFC closeout note + status flip to Implemented
```

### Wave 5 — Parallel, 4 sonnet subagents (post-v0.2 polish)

```
I. Phase 19 — Postgres parity benchmark harness (deferred head-to-head)
J. Phase 20 — hygiene audit (audit log, ACT-R cleanup cadence)
K. Phase 21 — lightweight model swap (Verifier 27B → 270M, ExtractorTier)
L. Phase 22 — SDK distribution (PyPI / npm release pipelines)
```

---

## 5. Alternatives Considered

| Alternative                                  | Rejected because                                                                                  |
|----------------------------------------------|---------------------------------------------------------------------------------------------------|
| `tenant: String` field, no newtype           | Silent cross-scope leaks survive type-checker; same pattern Mem0 / Zep ship and bleed reputation. |
| Defer to v0.3                                | Compounds breaking-change debt; OSS launch without multi-agent isolation is non-credible.        |
| `metadata["scope"]` JSON-only                | Indexing burden in Postgres (GIN over JSONB), no compile-time guard, can't underpin RLS.         |
| Hierarchical `Scope` tree                    | Useful but increases v0.2 surface; flat string handles 95% of agent / tenant cases.              |
| Moon workspace-per-scope (`?ws=`)            | Requires Moon team coord for workspace ACLs; keyspace prefix gets us there with current Moon.    |
| Add `&Scope` only to writes, not reads       | Read path leak surface is the same; asymmetry confuses adopters and breaks RLS uniformity.       |

---

## 6. Migration & Backwards Compatibility

**There is no on-the-wire compatibility between v0.1 and v0.2.** This is
explicit. Mitigations:

1. **Migration script** `tools/migrate-0.1-to-0.2/` reads v0.1 snapshots,
   applies `metadata.tenant` (or a configured default) as `scope`, writes
   v0.2 rows. Idempotent, dry-run flag.
2. **Reserved literal `_legacy`** — the migration script can stamp
   pre-existing rows with `scope='_legacy'` if no `metadata.tenant` is
   present. RLS policies treat `_legacy` as a normal scope; no special-case.
3. **Helios coordination** — Helios is the only known internal consumer; its
   v0.2 cutover lands in Wave 3 (subagent H). External adopters of v0.1 are
   warned via CHANGELOG and migration guide.
4. **No deprecation shim.** A `Lunaris::ingest_unscoped` compatibility shim
   was considered and rejected — it preserves the leak surface this RFC
   exists to close.

---

## 7. Risks & Open Questions

| Risk                                                     | Mitigation                                                                  |
|----------------------------------------------------------|-----------------------------------------------------------------------------|
| Moon FT index count explodes for tenant-rich workloads   | Document `max_scopes_recommended` cap; future RFC for workspace pooling.    |
| Per-scope queues leak file descriptors at high scope counts | Bounded `JoinSet`, idle-scope timeout (30 min default) sheds dormant tasks. |
| Subagents diverge on scope-validation regex              | RFC fixes regex `^[A-Za-z0-9_\-:.]{1,128}$` verbatim; any deviation is a CI failure. |
| Wave 1E Moon partitioning + Wave 1B Postgres RLS race    | Both implement to RFC §3.4 trait shape; storage tests run independently per backend. |
| Performance regression on hot path from scope arg        | `SmolStr` clone is O(1) for short strings; criterion baseline taken on stub commit, regression budget = +5% p50. |
| Helios UAT discovers scope ergonomics issues post-Wave 1 | Wave 2 owner gate is explicitly for this; can rewind one subagent's work without re-running Wave 1.            |

**Open questions** (non-blocking — answer during Wave 1 in line with this RFC):

- Should `Scope::new("")` succeed and mean "default"? **No** — empty scopes
  are a footgun; an explicit `Scope::default_dev()` test helper is fine but
  not a production type.
- Should `serde_json::Value` metadata be allowed to redundantly carry a
  `scope` key? **No** — strip on serialize-in (defensive), refuse on
  deserialize-in if it disagrees with the typed `scope`.

---

## 8. Decision Log

- **2026-05-10** — Author elected aggressive Moon-Scope inclusion in v0.2
  (advisor recommended deferring to v0.2.5; author accepts timeline risk).
  Wave 1 subagent count expanded from 4 to 5.
- **2026-05-10** — RFC 0002 (DeterministicConsolidator) withdrawn —
  ACT-R is already deterministic per Anderson 1996 in
  `crates/lunaris-consolidate/src/act_r.rs`.
- **2026-05-10** — Spawn gate confirmed: this RFC ⇒ stub commit ⇒ Wave 1
  parallel. No subagent runs against a moving signature.
- **2026-05-11** — `KeywordPort` gained `&Scope` in Wave 2.5A (overlooked in
  Wave 0 type freeze — §3.4 covered `StoragePort`'s 8 methods but missed the
  BM25 extension trait). See §3.4 amendment above.

---

## 9. Acceptance

This RFC was **Accepted** at v0.2 planning time and **Implemented** in v0.2.0
on 2026-05-11. Status flipped after Wave 4 close-out.

## 10. Implementation Outcome (v0.2.0 close-out)

Shipped across six waves on branch `v0.2/scope`, base commit `cace8bc`:

| Wave | Scope | Key commits |
|------|-------|------------|
| **0** — Type freeze | `Scope` newtype, primitive scope fields, `StoragePort` signature change, `ScopedLunaris<'a>` stub, `AuthClaims.scope` | `cace8bc` |
| **1** — Parallel implementation | A: core hardening / B: Postgres RLS / C: Moon per-scope keyspace / D: ScopedLunaris bodies + EpisodeBuilder security fix / E: server routes | `4fd5e1f`, `664bb93`, `d266593`, `f674b95`, `5e5ca34`, `0e92ff0`, `6ec6bb9` |
| **2** — Migration docs + ergonomics review | `docs/migration/0.1-to-0.2.md`, `cargo public-api` diff dumps | `bd6d36d` |
| **2.5** — Carry-over debt | B: keyspace helpers moved to `lunaris-core` / A: `KeywordPort` gains `&Scope` (§3.4 amendment) / C: scope plumbed through `QueryContext` and operators / D: regression test pinning the contract | `7cbdf50`, `e74095b`, `e056aaf`, `c71bc96` |
| **3** — Parallel completion | F: per-scope `ConsolidateSupervisor` / `VerifySupervisor` / G: PyO3 + napi SDK regen / H: HTTP multi-agent UAT contract + `docs/multi-agent.md` | `b273381`, `cbd8d3e`, `c2018c1`, `f8b228b` |
| **4** — Close-out | CHANGELOG, RFC status flip, migration guide finalization, `recall_graph_mode` regression fix | this commit |

**Outcome vs RFC contract:**

- ✅ Every primitive carries `Scope` at the type level
- ✅ `ScopedLunaris<'a>` typestate makes cross-scope ingest a compile error
  via `EpisodeBuilder::into_episode` being `pub(crate)` to `lunaris`
- ✅ `StoragePort` and `KeywordPort` both partition by `&Scope`
- ✅ Postgres RLS + non-superuser role requirement documented; cross-scope
  leak test green
- ✅ Moon per-scope keyspace (`lunaris:{scope}:`) + per-scope FT/GRAPH/MQ
  resources; soft cap of 512 scopes per Moon node surfaced via
  `StorageCapabilities.max_scopes_recommended`
- ✅ HTTP 422 rejects `metadata.tenant` and top-level `scope` body overrides
- ✅ Per-scope consolidator/verifier supervisors with bounded concurrency
  and panic isolation
- ✅ PyO3 and napi SDK bindings expose the v0.2 API
- ✅ External-consumer UAT contract in `docs/multi-agent.md` + executable
  `crates/lunaris-server/tests/multi_agent_uat.rs`
- ⏭️ `forget` per-scope routing — deferred to v0.3 (`ScopedLunaris::forget`
  with target HTTP 403/404 contract pinned in an `#[ignore]`'d UAT-4)
- ⏭️ Pipeline handles (`ConsolidatorPipelineHandle`, `VerifyPipelineHandle`)
  using deprecated single-topic workers — supervisor migration deferred to
  v0.3 (`#[allow(deprecated)]` at the call sites with a v0.3 TODO)

**Verifier gates (close-out, on v0.2/scope HEAD):**

- `cargo build --workspace --exclude lunaris-py --exclude lunaris-ts --all-features` ✅
- `cargo clippy ... -- -D warnings` ✅
- `cargo fmt --check` ✅
- `cargo test --workspace --exclude lunaris-py --exclude lunaris-ts --all-features` ✅ (offline; live `moon_url_*` / `postgres_url_*` UAT require running services)
- `cargo test -p lunaris-server --test multi_agent_uat` ✅ (5/5 UAT scenarios)
- INGEST-04 invariant: single `atomic_write` at `crates/lunaris-ingest/src/pipeline.rs:116` ✅
- `maturin develop` + pytest ✅ (14/14)
- `napi build` + vitest ✅ (50/50)

RFC 0001 is **Implemented**. Carry-over items are tracked in CHANGELOG
§"Known issues / v0.3 carryover" and `docs/migration/0.1-to-0.2.md` §10.
