# Phase 14 — ReflectOutput Storage-Side Application

**TL;DR.** `Lunaris::end_turn` (handle.rs:507) returns a `ReflectOutput` from the reflection LLM but discards all three fields after a `tracing::info!` log. Phase 14 wires them into the system: `invalidate` triggers per-ulid MVCC `bt.sys.1` stamping via a new `apply_reflect_invalidate` helper (not `apply_supersede` — reflect has no winner/loser pair); `boost` populates an ephemeral per-handle `parking_lot::RwLock<LruCache<(Scope, Ulid), f32>>` consumed by a new post-hydrate pass in the retrieval pipeline; `pre_warm_query` spawns a fire-and-forget tokio task bounded by a per-handle semaphore. Each sub-phase is additive and independently shippable. The load-bearing constraint is that `end_turn` currently carries no `Scope`, so the first design decision is whether to add `Scope` to `Lunaris::end_turn` or migrate the call surface to `ScopedLunaris::end_turn`.

---

## 1. Current State

`Lunaris::end_turn` signature (handle.rs:507):

```rust
pub async fn end_turn(&self, input: ReflectInput) -> Result<ReflectOutput, LunarisError>
```

The body (handle.rs:509–518) calls `self.reflect_supervisor.reflect(input)` and logs the result. No storage write occurs. No retrieval weight is updated. `ReflectOutput` is returned to the caller, who today has no public API to apply it.

`ReflectInput` fields (reflect.rs:52–66):
- `recent_fact_ids: Vec<Ulid>` — caller-nominated facts surfaced this turn.
- `recent_chunk_ids: Vec<Ulid>` — caller-nominated load-bearing chunk hits.

`ReflectOutput` fields (reflect.rs:72–83):
- `invalidate: Vec<Ulid>` — facts to MVCC-supersede.
- `boost: Vec<Ulid>` — chunks to promote in future retrieval scoring.
- `pre_warm_query: Option<String>` — speculative next-turn query string.

The type contract in reflect.rs:59–65 constrains which kind each field applies to: `invalidate` operates on `recent_fact_ids` (→ `fact:<ulid>` KV keys); `boost` operates on `recent_chunk_ids` (→ `chunk:<ulid>` KV keys). The LLM cannot nominate a ulid it was not given, so no cross-kind ambiguity is possible at the reflect layer. Phase 14 takes this as a hard precondition.

---

## 2. Field: `invalidate: Vec<Ulid>` — MVCC Supersede

### 2.1 Mechanism Overview

The verify worker uses `apply_supersede` (worker.rs:305) for contradiction arbitration. That function requires a `VerifyDecision { winner_id, loser_id }` pair and emits two `WriteOp::KvPut`s in one `atomic_write`. Reflect invalidation has no winner — it is a unilateral tombstone: stamp `bt.sys.1 = Some(now)` on the target row and write back. A new helper is required.

**New helper — proposed signature:**

```rust
// crates/lunaris-verify/src/reflect_apply.rs  (new file, ~80 LOC)
pub(crate) async fn apply_reflect_invalidate(
    storage: &Arc<dyn StoragePort>,
    scope: &Scope,
    ulid: Ulid,
    clock: &Arc<HlcClock>,
) -> Result<(), LunarisError>
```

Implementation steps (mirroring `apply_supersede` at worker.rs:282–451):

1. Derive key: `format!("fact:{ulid}").into_bytes()`. This matches the `fact_key` emitted by `lunaris_core::keyspace` and used in `ingest.rs`. (reflect.rs:59–63 contracts that `invalidate` only carries `recent_fact_ids`, so the `"fact:"` prefix is unconditional in Phase 14.1.)
2. `let now = clock.tick()`.
3. `storage.read_as_of(scope, &key, now).await` — load the row.
4. **Idempotency guard**: if `row.bt.sys.1.is_some()`, the row is already invalidated. Increment counter `reflect_invalidate_skipped_already_invalid_total`. Return `Ok(())`.
5. Mutate: `bt.invalidate_sys(now)` (bitemporal.rs:32).
6. JSON-patch `payload["bt"]` — same pattern as worker.rs:349–368. This is mandatory (B-2-RESIDUAL: `WriteOp::KvPut` has no separate `bt` field; Moon HSET and Postgres both derive bt from the serialized payload bytes).
7. `storage.atomic_write(scope, &[WriteOp::KvPut { key, value: patched_bytes }]).await` — **exactly ONE `atomic_write` per ulid** (D-11 invariant).

**Batch fan-out in `end_turn`:** Phase 14.1 calls `apply_reflect_invalidate` for each ulid in `output.invalidate` sequentially (simple loop). Parallel fan-out via `JoinSet` is a Phase 14.1 extension — deferred because the per-ulid cost is one `read_as_of` + one `atomic_write` and the expected batch size from a 384-token reflect budget is at most ~20 ulids.

**Missing-row handling:** If `read_as_of` returns `None` (fact not present in scope), log `tracing::warn!(ulid = %ulid, "reflect_invalidate_fact_not_found")` and continue. Do NOT synthesize a tombstone row — reflect is advisory; a not-found ulid is not an error condition.

**Last-writer semantics:** If two concurrent `end_turn` calls invalidate the same ulid, the second `atomic_write` overwrites the first. Both stamp `bt.sys.1 = Some(now_N)`. The later timestamp wins — this is the standard MVCC last-writer-wins for `sys.1`. Neither call is wrong; both correctly express "this fact is no longer believed."

### 2.2 Scope Source

`Lunaris::end_turn` currently carries no `Scope` (handle.rs:507). Applying `invalidate` requires a scope. Two options:

**Option A — Add `Scope` to `Lunaris::end_turn`:**

```rust
pub async fn end_turn(
    &self,
    scope: &Scope,
    input: ReflectInput,
) -> Result<ReflectOutput, LunarisError>
```

Breaking change to the public API. Simplest to reason about.

**Option B — Add `ScopedLunaris::end_turn`:**

```rust
// on ScopedLunaris<'a> (handle.rs:666)
pub async fn end_turn(&self, input: ReflectInput) -> Result<ReflectOutput, LunarisError>
```

`self.scope` is already the JWT-bound partition key. `Lunaris::end_turn` stays as-is (no scope, advisory-only, no storage writes — or deprecated). Mirrors the pattern established by `ScopedLunaris::recall` and `ScopedLunaris::ingest`. This is the recommended option: it enforces the CLAUDE.md constraint that the JWT tenant claim is the only source for the partition key without requiring callers to thread a scope they already have into an unscoped method.

### 2.3 Audit Event

`AuditEvent` (audit.rs:119, `#[non_exhaustive]`) currently has four variants with frozen fixture shapes. Adding a reflect-specific variant requires:

1. New enum arm: `ReflectInvalidation { ulid: String, scope: String, invalidated_at_iso: String }`.
2. New fixture file: `crates/lunaris-core/tests/fixtures/audit/v0.1.0/reflect_invalidation.json`.
3. Parity test update at `crates/lunaris-core/tests/audit_event_fixture_parity.rs`.

Wire: after each successful `apply_reflect_invalidate`, fire-and-forget publish of `AuditEvent::ReflectInvalidation { ulid: ulid.to_string(), scope: scope.to_string(), invalidated_at_iso }` to `AUDIT_TOPIC` (`__lunaris_audit__`). Same `tracing::warn!`-on-error, never-propagate semantics as `publish_arbitration_audit` (worker.rs:460–478).

### 2.4 LOC Estimate (Phase 14.1)

| File | Change | LOC |
|---|---|---|
| `crates/lunaris-verify/src/reflect_apply.rs` | New helper `apply_reflect_invalidate` | ~80 |
| `crates/lunaris/src/handle.rs` | `ScopedLunaris::end_turn` or add `scope` param | ~40 |
| `crates/lunaris-core/src/audit.rs` | Add `ReflectInvalidation` variant + mirror types | ~20 |
| `crates/lunaris-core/tests/` | Fixture + parity test | ~30 |
| `crates/lunaris/tests/reflect_invalidate_smoke.rs` | Integration test (Moon + Postgres) | ~120 |
| **Total** | | **~290 LOC** |

---

## 3. Field: `boost: Vec<Ulid>` — Retrieval-Rank Adjustment

### 3.1 Mechanism Evaluation

Three options were considered:

**Option A — Persistent boost column.** Add `boost_score: f32` to the KV payload and surface it in Moon FT index scoring. Cost: schema migration on both Moon and Postgres, Moon FT index rebuild, MVCC interaction (does a boost write create a new sys version?), and cross-crate changes to `StoragePort`. Disproportionate for an advisory signal.

**Option B — Ephemeral in-memory LruCache on the Lunaris handle (recommended).** A `parking_lot::RwLock<LruCache<(Scope, Ulid), f32>>` on the `Lunaris` struct, populated by `end_turn`, consumed as a post-hydrate rescorer pass in `RetrievalBuilder::execute`. Runs entirely in Rust, after hydrate, before the `Vec<Hit>` is returned. Same placement as the existing recency rescorer (`rescore_recency` at builder.rs:296–308).

**Option C — Preferred-candidate promotion.** Treat boosted ulids as mandatory top-N inclusions regardless of retrieval score. Breaks RRF score semantics and causes unpredictable rank skew when a boosted chunk falls outside the query's natural recall set.

**Recommendation: Option B.** Mirrors the recency rescorer pattern already proven in the codebase. No storage migration. Cache lives on `Lunaris` (per-handle, per-deployment), keyed by `(Scope, Ulid)` to respect tenant isolation. Eviction is time-based (e.g., TTL 5 minutes) or count-based (LRU capacity 10_000 per handle).

### 3.2 Implementation

**New field on `Lunaris`:**

```rust
// crates/lunaris/src/handle.rs, inside struct Lunaris
pub(crate) boost_cache: Arc<parking_lot::RwLock<lru::LruCache<(Scope, Ulid), f32>>>,
```

Capacity knob: `LUNARIS_BOOST_CACHE_CAPACITY` env var, default `10_000`. Lock discipline: read the cache under `parking_lot::RwLock::read()`, snapshot to a local `HashMap<Ulid, f32>`, drop the guard, then proceed with the `.await` chain — never hold the lock across an await (CLAUDE.md invariant).

**Population in `end_turn` (or `ScopedLunaris::end_turn`):**

```rust
// After reflect_supervisor.reflect(input) returns output:
{
    let mut cache = self.boost_cache.write();
    for ulid in &output.boost {
        cache.put((scope.clone(), *ulid), BOOST_DELTA);  // BOOST_DELTA = 0.25f32
    }
}  // guard dropped before next .await
```

`BOOST_DELTA` is a named constant, not a magic number. Make it configurable later.

**Consumption in `RetrievalBuilder::execute` (builder.rs:246):**

Add a `boost_cache: Option<Arc<parking_lot::RwLock<lru::LruCache<(Scope, Ulid), f32>>>>` field on `RetrievalBuilder`. Seeded from the handle in `Lunaris::recall()` / `ScopedLunaris::dsl()`. In `execute`, after `hydrate` and before returning:

```rust
if let Some(cache) = &self.boost_cache {
    let snapshot: HashMap<Ulid, f32> = {
        let guard = cache.read();
        self.hits.iter()
            .filter_map(|h| guard.peek(&(ctx.scope.clone(), h.id)).map(|&v| (h.id, v)))
            .collect()
    };  // guard dropped here — no await after this
    for hit in &mut hits {
        if let Some(&delta) = snapshot.get(&hit.id) {
            hit.score += delta;
        }
    }
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}
```

**Why post-hydrate, not Moon-native?** The Moon-native one-round-trip path (`fuse_via_moon_native` in fusion.rs) runs inside `Retriever::retrieve` before hydrate. The boost adjustment is a Rust-layer scoring delta applied to `Vec<Hit>` — same layer as `rescore_recency` (builder.rs:296). It cannot reach into the Moon FT scoring engine without a schema change. The recommendation is explicitly post-hydrate.

**Multi-instance coherence:** The `boost_cache` is per-handle, in-process only. In a horizontally-scaled Lunaris deployment (multiple processes), each process has an independent cache. Boost nominations from one process's `end_turn` do not propagate to other processes. This is acceptable for v0: the use case is single-process agent loops. Multi-process coherence (e.g., publishing boost events to a shared topic like `__lunaris_boost__`) is a Phase 14.2 follow-up decision for the lead.

### 3.3 LOC Estimate (Phase 14.2)

| File | Change | LOC |
|---|---|---|
| `crates/lunaris/src/handle.rs` | Add `boost_cache` field, seed in `recall()` | ~30 |
| `crates/lunaris-retrieve/src/builder.rs` | Add cache field, post-hydrate apply pass | ~40 |
| `crates/lunaris/tests/reflect_boost_smoke.rs` | Integration test | ~80 |
| `Cargo.toml` (lunaris crate) | Add `lru` dependency (confirmed not in tree via `cargo tree -p lunaris`) | ~2 |
| **Total** | | **~152 LOC** |

---

## 4. Field: `pre_warm_query: Option<String>` — Speculative Recall Warm-Up

### 4.1 Mechanism

`pre_warm_query` is a string the reflection LLM predicts will be the agent's next query. The goal is to populate Moon's FT cache (or the OS page cache for Postgres) before the agent issues the actual query, reducing the first-hit latency on the next turn.

**Execution model: fire-and-forget tokio task with semaphore.**

Synchronous execution (block `end_turn` on the warm-up) is ruled out: a warm-up that takes >50ms would stall the agent's next-turn start and defeat the purpose. Pure fire-and-forget with no concurrency bound would allow a flapping reflector to fan out unbounded warm-up tasks.

Recommended: spawn one task per `end_turn` call, gated by a per-handle semaphore of capacity 4. If the semaphore is exhausted (too many concurrent warm-ups in flight), skip the warm-up and log a warning rather than blocking.

**New field on `Lunaris`:**

```rust
pub(crate) warm_up_semaphore: Arc<tokio::sync::Semaphore>,
```

Default: `Semaphore::new(4)`. Knob: `LUNARIS_PREWARM_CONCURRENCY` env var.

**Logic in `end_turn` (or `ScopedLunaris::end_turn`), after the reflect call:**

```rust
if let Some(query_str) = output.pre_warm_query.clone() {
    match self.warm_up_semaphore.clone().try_acquire_owned() {
        Ok(permit) => {
            let storage = self.storage.clone();
            let embedder = self.embedder.clone();
            let keyword = self.keyword.clone();
            let scope = scope.clone();
            tokio::spawn(async move {
                let _permit = permit;  // released when task ends
                let query = lunaris_retrieve::Query::text(&query_str);
                // Run the default recall root (Vector top-30) — warm-up
                // is best-effort; errors are logged, never propagated.
                let result = RetrievalBuilder::from_handle(storage, keyword, embedder)
                    .with_scope(scope)
                    .execute(query)
                    .await;
                match result {
                    Ok(hits) => tracing::debug!(
                        hits = hits.len(), query = %query_str, "pre_warm_complete"
                    ),
                    Err(e) => tracing::warn!(
                        err = %e, query = %query_str, "pre_warm_failed"
                    ),
                }
            });
        }
        Err(_) => {
            tracing::debug!(query = %query_str, "pre_warm_skipped_semaphore_full");
        }
    }
}
```

**Cache topology:** The warm-up issues a real `RetrievalBuilder::execute` call. For Moon, this means the FT index results are loaded into Moon's in-process page cache (the Moon instance owns the memory). For Postgres, OS page cache warm. There is no Lunaris-internal warm-up cache — the benefit comes from the backend caches, not from storing the results in Rust heap.

**Measuring effectiveness:** Warm-up hit-rate is measurable by adding a `pre_warm_active: bool` field to `Hit` (or via a Prometheus counter `lunaris_prewarm_hit_total` incremented when a recall query's result set intersects the last warm-up scope). The cheapest experiment: log the intersection cardinality between the warmed result ulids and the next actual recall result ulids. Set a 5-minute TTL on warm-up results in the experiment log. If intersection rate >40% across 1000 turns, the warm-up mechanism earns its latency budget.

### 4.2 LOC Estimate (Phase 14.3)

| File | Change | LOC |
|---|---|---|
| `crates/lunaris/src/handle.rs` | Add `warm_up_semaphore`, fire-and-forget in `end_turn` | ~50 |
| `crates/lunaris/tests/reflect_prewarm_smoke.rs` | Integration smoke (check no panic, semaphore bound) | ~60 |
| **Total** | | **~110 LOC** |

---

## 5. Cross-Cutting Concerns

### 5.1 Tenant Scope Safety

`Lunaris::end_turn` (handle.rs:507) currently takes no `Scope`. CLAUDE.md states: "The JWT `tenant` claim is the **only** source of truth for the partition scope." The `ScopedLunaris` wrapper (handle.rs:661) already enforces this for ingest and recall by binding scope at construction time from the JWT-decoded tenant claim in the server layer.

**Phase 14 requirement:** `ScopedLunaris::end_turn` must be the production entry point for all three apply-paths. `Lunaris::end_turn` may remain for backwards compatibility but MUST NOT apply any storage writes without a scope — it should stay advisory-only (log + return) or be deprecated with a `scope-dev-allowed` marker identical to `Lunaris::recall()` at recall.rs:78–82.

The `(Scope, Ulid)` composite key on the boost cache is the type-level expression of this invariant: a boost for `scope="agent-42"` cannot leak into `scope="agent-99"` recall.

### 5.2 Audit Pipeline Integration (D-22)

Today `__lunaris_audit__` carries three event kinds from two crates: `Forget` (lunaris/src/forget.rs), `VerifierArbitration` (lunaris-verify/src/worker.rs), and the two consolidator events. Phase 14.1 adds `ReflectInvalidation`. The source field differentiates it from `VerifierArbitration` at the ops-triage layer — grep queries like `jq 'select(.kind=="ReflectInvalidation")'` on the audit stream.

The audit fixture parity test at `crates/lunaris-core/tests/audit_event_fixture_parity.rs` MUST be extended for the new variant before Phase 14.1 ships. The fixture must be committed alongside the production code, not in a follow-up. This is a CI gate, not optional.

`boost` writes no audit event in Phase 14 — boosting is ephemeral and non-destructive. `pre_warm_query` is fire-and-forget with no side effects on stored facts; no audit entry needed.

### 5.3 Failure-Mode Discipline

`ReflectOutput` docstring (reflect.rs:68–70) states: "All three fields are advisory — nothing in this commit applies them." Phase 14 applies them but must preserve the advisory contract: reflect-driven invalidations MUST NOT fail the agent's next turn.

Recommended failure semantics:
- A single-ulid `apply_reflect_invalidate` failure (storage error, parse error) is logged via `tracing::warn!` and skipped. The remaining ulids in the batch continue. This mirrors the verify worker's per-message error handling (worker.rs:82–85: "errors are logged but do NOT propagate").
- A `boost_cache` write failure (lock poisoned — impossible with `parking_lot`) is unreachable in practice.
- A `pre_warm_query` task panic is contained by `tokio::spawn` task boundaries and logged on `JoinHandle` poll — `end_turn` does not hold the handle, so there is nothing to poll. Add a `tracing::warn!` inside the task's panic handler (via `std::panic::catch_unwind` is not needed — tokio tasks catch panics at the executor level).

There is no rollback for a partial-apply invalidate batch. If three of five ulids are successfully invalidated before a storage outage kills the fourth, the three remain invalidated. This is correct for an advisory pass: partial application is better than no application. Document this as "best-effort batch, no transactional rollback" in the `apply_reflect_invalidate` rustdoc.

### 5.4 Observability

All counters and histograms named with the `lunaris_reflect_` prefix for easy dashboard grouping:

| Instrument | Kind | Description |
|---|---|---|
| `lunaris_reflect_invalidate_total` | counter | Total fact ulids processed for invalidation |
| `lunaris_reflect_invalidate_applied_total` | counter | Actually stamped (bt.sys.1 set) |
| `lunaris_reflect_invalidate_skipped_already_invalid_total` | counter | Idempotent skip (already had sys.1) |
| `lunaris_reflect_invalidate_not_found_total` | counter | Ulid not found in scope |
| `lunaris_reflect_invalidate_error_total` | counter | Storage error during invalidate |
| `lunaris_reflect_boost_cache_size` | gauge | Current LRU entry count (sampled) |
| `lunaris_reflect_boost_applied_per_recall_total` | counter | Recall calls where ≥1 hit received a boost delta |
| `lunaris_reflect_prewarm_spawned_total` | counter | Fire-and-forget tasks spawned |
| `lunaris_reflect_prewarm_skipped_semaphore_full_total` | counter | Skipped due to semaphore exhaustion |
| `lunaris_reflect_prewarm_hit_total` | counter | Warm-up result ulid appeared in subsequent recall |
| `lunaris_reflect_end_turn_duration_ms` | histogram | Total `end_turn` wall time (incl. LLM call) |

Tracing spans: wrap the invalidate fan-out in `tracing::info_span!("lunaris.reflect.invalidate", turn_id = ?turn_id)` and the pre-warm task in `tracing::info_span!("lunaris.reflect.prewarm", query = %query_str)`.

### 5.5 Test Plan

**Phase 14.1 — Invalidate integration test** (`crates/lunaris/tests/reflect_invalidate_smoke.rs`):

Mirror the structure of `crates/lunaris/tests/verify_pipeline_smoke.rs::verify_apply_supersede_writes_real_mvcc_rows` (line 483):

1. Open a `MoonStorage` handle against the test Moon instance (Moon first, per CLAUDE.md).
2. Ingest one episode with a known fact ulid.
3. Call `apply_reflect_invalidate(storage, scope, fact_ulid, clock)` directly (bypass the LLM; test the helper in isolation).
4. Assert `storage.read_as_of(scope, &fact_key, now + 1)` returns a row where `bt.sys.1 == Some(now)`.
5. Assert the JSON payload at `payload["bt"]["sys"][1]` is non-null and matches the expected Hlc.
6. Call `apply_reflect_invalidate` again on the same ulid — assert the idempotency counter incremented and exactly ONE `atomic_write` occurred in total (no second write for an already-invalid row).

**Postgres parity test:** Identical scenario via `PostgresStorage`. Gate behind `#[cfg(feature = "test-postgres")]` consistent with the rest of the codebase.

**Phase 14.2 — Boost integration test** (`crates/lunaris/tests/reflect_boost_smoke.rs`):

1. Ingest two chunks, score them with a fused recall.
2. Call `ScopedLunaris::end_turn` with a `ReflectInput` nominating the lower-scoring chunk in `recent_chunk_ids`.
3. Assert the boost-nominated chunk's `hit.score` is `>= original_score + BOOST_DELTA` in the next `ScopedLunaris::recall()` call.
4. Assert the non-nominated chunk's score is unchanged.

**Phase 14.3 — Pre-warm smoke test** (`crates/lunaris/tests/reflect_prewarm_smoke.rs`):

1. Set `LUNARIS_PREWARM_CONCURRENCY=1`.
2. Call `end_turn` twice in rapid succession with `pre_warm_query = Some("Alice")`.
3. Assert the second call logs `pre_warm_skipped_semaphore_full` (semaphore exhausted after first acquire).
4. Assert no panics and `end_turn` returns in <100ms (the warm-up task is not awaited).

---

## 6. Phased Rollout

### Phase 14.1 — Invalidate Path (highest value, smallest surface)

**Files touched:**
- `crates/lunaris-verify/src/reflect_apply.rs` — new file, `apply_reflect_invalidate` helper
- `crates/lunaris-verify/src/lib.rs` — re-export `apply_reflect_invalidate` (pub for integration test)
- `crates/lunaris/src/handle.rs` — add `Scope` to `Lunaris::end_turn` OR add `ScopedLunaris::end_turn`; call the new helper in a sequential loop
- `crates/lunaris-core/src/audit.rs` — add `ReflectInvalidation` variant
- `crates/lunaris-core/tests/fixtures/audit/v0.1.0/reflect_invalidation.json` — new fixture
- `crates/lunaris-core/tests/audit_event_fixture_parity.rs` — update parity test
- `crates/lunaris/tests/reflect_invalidate_smoke.rs` — integration test

**LOC:** ~290  
**Risk:** Low. Single `atomic_write` per ulid. No schema change. Idempotent. Advisory failure semantics.

### Phase 14.2 — Boost Path (per-handle ephemeral LRU)

**Files touched:**
- `crates/lunaris/src/handle.rs` — add `boost_cache` field, populate in `end_turn`; seed `RetrievalBuilder` in `recall()`
- `crates/lunaris-retrieve/src/builder.rs` — add `boost_cache` field, post-hydrate apply pass after recency rescorer (builder.rs:296–308)
- `Cargo.toml` (lunaris crate) — `lru` dependency (already transitive? check)
- `crates/lunaris/tests/reflect_boost_smoke.rs` — integration test

**LOC:** ~152  
**Risk:** Medium. Adds state to `Lunaris` struct (must not regress `Clone` impls). The `Arc<RwLock<LruCache>>` is `Clone`-cheap. No lock-across-await risk if snapshot pattern is followed. Boost cache is per-process — no cross-instance propagation.

### Phase 14.3 — Pre-warm Query (fire-and-forget)

**Files touched:**
- `crates/lunaris/src/handle.rs` — add `warm_up_semaphore` field; fire-and-forget spawn in `end_turn`
- `crates/lunaris/tests/reflect_prewarm_smoke.rs` — smoke test

**LOC:** ~110  
**Risk:** Low-medium. The spawned task holds no shared mutable state. Semaphore bounds the fan-out. Moon's FT cache warm-up benefit is not quantified until the experiment from §4.2 runs.

---

## 7. Open Questions for Project Lead

1. **API surface for scope in `end_turn`:** Should `Lunaris::end_turn` gain a `scope: &Scope` parameter (breaking change), or should `ScopedLunaris::end_turn` be the production entry point and `Lunaris::end_turn` remain advisory-only (no storage writes, backwards compatible)? The `ScopedLunaris` path is recommended but requires callers already on the scoped API.

2. **Reflect-driven supersede in `__lunaris_audit__`:** Should `apply_reflect_invalidate` publish to `__lunaris_audit__` with `kind: "ReflectInvalidation"`? This adds the fixture/parity-test work (~50 LOC) but gives ops a complete supersede history across both verify-driven and reflect-driven invalidations. Alternative: log-only (no audit record) for Phase 14.1 and add audit in v0.3.

3. **Boost persistence vs. ephemeral cache:** The recommendation is ephemeral `LruCache` (Option B, §3.1). If the product requires boost signals to survive process restarts or propagate across horizontally-scaled instances, a persistent storage path (new `boost_score` column or a `__lunaris_boost__` queue) is required. Confirm the v0 scope is single-process before committing to the ephemeral design.

4. **Per-tenant rate-limit on `end_turn` invalidations:** Should there be a configurable cap on how many ulids a single `end_turn` call can invalidate (e.g., `LUNARIS_REFLECT_MAX_INVALIDATE=50`)? Without a cap, a pathological reflection LLM response could fan out hundreds of `read_as_of` + `atomic_write` pairs in one call. A hard cap with a `tracing::warn!` on truncation is low-cost insurance.

5. **`pre_warm_query` recall depth:** What operator root should the warm-up use? The current proposal uses the default `Vector::new("chunks", 30)`. Should it mirror whatever operator root the `ScopedLunaris` handle was last configured with, or always use the default? Mirroring requires storing the last operator config on the handle (added state); defaulting is simpler but may warm the wrong index for callers using a custom root.

6. **Boost `BOOST_DELTA` configurability:** `0.25f32` is an arbitrary initial constant. Should this be per-handle configurable via `LUNARIS_REFLECT_BOOST_DELTA` env var, or left as a compile-time constant until empirical data from production informs the right value?
