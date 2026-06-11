//! Phase 14.1 — `apply_reflect_invalidate`: MVCC unilateral tombstone for
//! reflect-nominated facts.
//!
//! ## Invariants upheld
//!
//! - **D-11 (one `atomic_write` per logical write):** all `WriteOp::KvPut`
//!   entries for this batch are collected into a single slice and committed in
//!   one `storage.atomic_write(scope, &ops)` call.  There is no loop of
//!   one-shot writes; the function signature enforces this at the call-site.
//!
//! - **D-22 (every supersede publishes one audit record):** after the single
//!   `atomic_write` succeeds, one `AuditEvent::ReflectInvalidation` is
//!   published per successfully-stamped ulid via
//!   `lunaris_core::audit::publish_audit_event` (fire-and-forget; never
//!   propagates).
//!
//! - **B-2-RESIDUAL (JSON-patch):** `WriteOp::KvPut` carries no separate `bt`
//!   field.  Moon HSET and Postgres both derive the persisted bt from the
//!   serialised payload bytes.  Every bt mutation MUST ride inside the
//!   `WriteOp::KvPut.value` bytes — identical pattern to
//!   `lunaris_verify::worker::apply_supersede`.
//!
//! ## Failure-mode contract
//!
//! This function is best-effort.  Missing rows and already-invalidated rows
//! are silently skipped (with `tracing::warn!`).  If `atomic_write` fails the
//! error is returned; the caller (`ScopedLunaris::end_turn`) logs it and
//! continues — a reflect-driven invalidation failure must NOT fail the agent
//! turn.
//!
//! Partial application is possible: if three of five ulids are successfully
//! added to the batch but a later `read_as_of` panics (which the type system
//! prevents) or the payload cannot be parsed (which returns `Err`), only the
//! rows added up to that point are written.  Per-row parse errors cause the
//! row to be skipped rather than aborting the batch.

use std::num::NonZeroUsize;
use std::sync::Arc;

use lunaris_core::audit::{AuditEvent, publish_audit_event};
use lunaris_core::{HlcClock, LunarisError, Scope, StoragePort, WriteOp};
use ulid::Ulid;

// ── Phase 14.2 ─────────────────────────────────────────────────────────────

/// Score delta applied to every boosted chunk on each retrieval call.
///
/// 0.25 was chosen so a single boost lifts a 0.70 cosine-similarity chunk
/// above a 0.90 chunk after one `end_turn` call (0.70 + 0.25 = 0.95 > 0.90).
/// It is not configurable per-call to keep the cache entry shape simple
/// (the stored value IS the delta; no per-entry config is needed).
pub const BOOST_DELTA: f32 = 0.25;

/// Populate the per-handle boost cache from `ReflectOutput::boost`.
///
/// ## Semantics
///
/// Writes `(scope.clone(), *ulid) → BOOST_DELTA` into the LRU cache for each
/// ulid in `chunk_ids`. This is a **replacement** (not additive): a second
/// `end_turn` boosting the same ulid stomps the prior entry.  Callers that
/// want additive accumulation should not use this helper.
///
/// ## Lock discipline
///
/// Acquires the write lock, writes all entries, then drops the guard.
/// No `.await` follows before the guard is released (the function is
/// synchronous), so the CLAUDE.md "never hold a lock across `.await`"
/// invariant is satisfied.
///
/// ## Capacity helper
///
/// The `boost_cache` must be constructed via [`boost_cache_capacity`] so
/// the `NonZeroUsize` capacity is always > 0.
pub fn apply_reflect_boost(
    boost_cache: &Arc<parking_lot::RwLock<lru::LruCache<(Scope, Ulid), f32>>>,
    scope: &Scope,
    chunk_ids: &[Ulid],
    delta: f32,
) {
    if chunk_ids.is_empty() {
        return;
    }
    let mut cache = boost_cache.write();
    for &ulid in chunk_ids {
        cache.put((scope.clone(), ulid), delta);
    }
    // write guard drops here — no .await follows
}

/// Return the LRU cache capacity from `LUNARIS_BOOST_CACHE_CAPACITY` env var,
/// defaulting to 10 000 when unset, empty, zero, or non-numeric.
///
/// Panics are impossible: the fallback is a hard-coded `NonZeroUsize::new(10_000)`.
pub fn boost_cache_capacity() -> NonZeroUsize {
    std::env::var("LUNARIS_BOOST_CACHE_CAPACITY")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .and_then(NonZeroUsize::new)
        // SAFETY: 10_000 is non-zero — this unwrap can never fail.
        .unwrap_or_else(|| NonZeroUsize::new(10_000).unwrap())
}

/// Apply reflect-driven MVCC invalidation for every ulid in `fact_ids`.
///
/// ## Batch semantics
///
/// All `WriteOp::KvPut` entries are batched into a single `atomic_write` call
/// (D-11).  Rows that are missing or already invalidated are skipped; the
/// remaining rows are committed together.
///
/// ## `clock` parameter
///
/// The caller MUST pass the same `HlcClock` instance that was used to stamp
/// the rows during ingest.  Using a fresh clock risks producing
/// `bt.sys.1 < bt.sys.0` (i.e., `sys.0 > sys.1`) when the ingest clock has
/// advanced its monotonic counter within the same wall-ms.  This mirrors
/// `apply_supersede` (worker.rs:305), which takes the engine's shared clock.
///
/// ## Return value
///
/// Returns the ulids that were successfully stamped so the caller can emit
/// one `AuditEvent::ReflectInvalidation` per success.
///
/// ## Best-effort partial failure
///
/// If a row's payload bytes cannot be parsed as JSON (corrupt row), that ulid
/// is skipped with a `tracing::warn!` rather than aborting the entire batch.
/// The `atomic_write` is called with whatever ops accumulated successfully.
/// If the `atomic_write` itself fails, the error is returned and NO audit
/// events are published.
pub async fn apply_reflect_invalidate(
    storage: &Arc<dyn StoragePort>,
    scope: &Scope,
    clock: &Arc<HlcClock>,
    turn_id: Option<Ulid>,
    fact_ids: &[Ulid],
) -> Result<Vec<Ulid>, LunarisError> {
    if fact_ids.is_empty() {
        return Ok(Vec::new());
    }

    let now = clock.tick();

    let mut ops: Vec<WriteOp> = Vec::with_capacity(fact_ids.len());
    let mut stamped: Vec<Ulid> = Vec::with_capacity(fact_ids.len());

    for &ulid in fact_ids {
        let key = format!("fact:{ulid}").into_bytes();

        let row = match storage.read_as_of(scope, &key, now).await.map_err(LunarisError::Storage)? {
            Some(r) => r,
            None => {
                tracing::warn!(
                    %ulid,
                    %scope,
                    "reflect_invalidate_fact_not_found; skipping"
                );
                continue;
            }
        };

        // Idempotency guard: if bt.sys.1 is already set, the row is already
        // invalidated.  Do not re-stamp; skip silently.
        if row.bt.sys.1.is_some() {
            tracing::debug!(
                %ulid,
                %scope,
                "reflect_invalidate_skipped_already_invalid"
            );
            continue;
        }

        // Mutate: stamp bt.sys.1 = Some(now).
        let mut bt = row.bt;
        bt.invalidate_sys(now);

        // JSON-patch: `WriteOp::KvPut` has no separate `bt` field.  The bt
        // mutation must ride inside the serialised payload bytes (B-2-RESIDUAL).
        let mut payload: serde_json::Value = match serde_json::from_slice(&row.value) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    %ulid,
                    %scope,
                    err = %e,
                    "reflect_invalidate_payload_parse_failed; skipping row"
                );
                continue;
            }
        };

        payload["bt"] = match serde_json::to_value(bt) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    %ulid,
                    %scope,
                    err = %e,
                    "reflect_invalidate_bt_serialize_failed; skipping row"
                );
                continue;
            }
        };

        let patched_bytes = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    %ulid,
                    %scope,
                    err = %e,
                    "reflect_invalidate_payload_reserialize_failed; skipping row"
                );
                continue;
            }
        };

        ops.push(WriteOp::KvPut { key, value: patched_bytes });
        stamped.push(ulid);
    }

    if ops.is_empty() {
        // Nothing to write — all rows were missing or already invalidated.
        return Ok(Vec::new());
    }

    // D-11: exactly ONE atomic_write per apply_reflect_invalidate invocation.
    storage.atomic_write(scope, &ops).await.map_err(LunarisError::Storage)?;

    // D-22: publish one ReflectInvalidation audit event per stamped ulid.
    // Fire-and-forget: never propagate audit publish failures.
    let invalidated_at_iso = chrono::Utc::now().to_rfc3339();
    let turn_id_str = turn_id.map(|u| u.to_string());

    for &ulid in &stamped {
        let event = AuditEvent::ReflectInvalidation {
            ulid: ulid.to_string(),
            scope: scope.to_string(),
            invalidated_at_iso: invalidated_at_iso.clone(),
            turn_id: turn_id_str.clone(),
        };
        // publish_audit_event logs on failure and returns Ok(0) — never panics.
        // The blanket `Publisher` impl is on `Arc<dyn StoragePort>` so we
        // clone the Arc reference (cheap) rather than dereferencing it.
        let _ = publish_audit_event(storage, event).await;
    }

    Ok(stamped)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream::BoxStream;
    use lunaris_core::{
        BiTemporal, CypherDialect, CypherQuery, Filter, GraphResult, Hlc, Lsn, QueueMsg, Row,
        StorageCapabilities, StorageError, StoragePort, VectorHit, WriteOp,
    };
    use parking_lot::Mutex;
    use std::collections::HashMap;

    // ── Minimal in-memory storage for unit-testing ────────────────────────────

    #[derive(Default)]
    struct TestStorage {
        rows: Mutex<HashMap<Vec<u8>, Row<Bytes>>>,
        writes: Mutex<Vec<Vec<WriteOp>>>,
        publishes: Mutex<Vec<Vec<u8>>>,
    }

    impl TestStorage {
        fn seed_row(&self, key: Vec<u8>, bt: BiTemporal, payload: serde_json::Value) {
            let bytes = serde_json::to_vec(&payload).unwrap();
            self.rows.lock().insert(key.clone(), Row { key, value: bytes.into(), bt });
        }

        fn write_count(&self) -> usize {
            self.writes.lock().len()
        }

        fn last_batch(&self) -> Vec<WriteOp> {
            self.writes.lock().last().cloned().unwrap_or_default()
        }
    }

    #[async_trait]
    impl StoragePort for TestStorage {
        async fn atomic_write(&self, _scope: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
            // Apply ops to the in-memory map so read_as_of sees updated rows.
            {
                let mut rows = self.rows.lock();
                for op in ops {
                    if let WriteOp::KvPut { key, value } = op {
                        let bt: BiTemporal = serde_json::from_slice::<serde_json::Value>(value)
                            .ok()
                            .and_then(|v| serde_json::from_value(v["bt"].clone()).ok())
                            .unwrap_or(BiTemporal::at(Hlc::ZERO, Hlc::ZERO));
                        rows.insert(
                            key.clone(),
                            Row { key: key.clone(), value: value.clone().into(), bt },
                        );
                    }
                }
            } // guard dropped before any .await
            self.writes.lock().push(ops.to_vec());
            Ok(Lsn { wall_ms: 1, counter: self.writes.lock().len() as u32 })
        }

        async fn read_as_of(
            &self,
            _scope: &Scope,
            key: &[u8],
            _as_of: Hlc,
        ) -> Result<Option<Row<Bytes>>, StorageError> {
            Ok(self.rows.lock().get(key).cloned())
        }

        async fn publish(
            &self,
            _scope: &Scope,
            _topic: &str,
            _partition: u16,
            payload: Bytes,
        ) -> Result<u64, StorageError> {
            self.publishes.lock().push(payload.to_vec());
            Ok(self.publishes.lock().len() as u64)
        }

        #[allow(clippy::too_many_arguments)]
        async fn vector_search(
            &self,
            _scope: &Scope,
            _index: &str,
            _query: &[f32],
            _k: usize,
            _filter: Option<&Filter>,
            _as_of: Option<Hlc>,
            _rerank: bool,
        ) -> Result<Vec<VectorHit>, StorageError> {
            Ok(vec![])
        }
        async fn graph_traverse(
            &self,
            _scope: &Scope,
            _query: &CypherQuery,
            _as_of: Option<Hlc>,
        ) -> Result<GraphResult, StorageError> {
            Ok(GraphResult::default())
        }
        async fn scan_range(
            &self,
            _scope: &Scope,
            _prefix: &[u8],
            _as_of: Option<Hlc>,
        ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
            use futures::stream;
            Ok(Box::pin(stream::empty()))
        }
        async fn subscribe(
            &self,
            _scope: &Scope,
            _group: &str,
            _topic: &str,
            _partition: u16,
        ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
            Err(StorageError::NotSupported("subscribe not used in unit test"))
        }
        fn capabilities(&self) -> StorageCapabilities {
            StorageCapabilities {
                bi_temporal_native: true,
                graph_native: false,
                rerank_native: false,
                queue_native: false,
                max_vector_dim: 768,
                native_rrf: false,
                max_scopes_recommended: 0,
                cypher_dialect: CypherDialect::Legacy,
                graph_decay_native: false,
            }
        }
    }

    fn make_fact_row(
        ulid: Ulid,
        already_invalid: bool,
    ) -> (Vec<u8>, BiTemporal, serde_json::Value) {
        let key = format!("fact:{ulid}").into_bytes();
        let bt = if already_invalid {
            let hlc = Hlc::ZERO;
            BiTemporal { valid: (hlc, None), sys: (hlc, Some(hlc)) }
        } else {
            BiTemporal::at(Hlc::ZERO, Hlc::ZERO)
        };
        let payload = serde_json::json!({
            "fact_text": "test fact",
            "ulid": ulid.to_string(),
            "bt": serde_json::to_value(bt).unwrap(),
        });
        (key, bt, payload)
    }

    fn dev_scope() -> Scope {
        Scope::dev()
    }

    fn test_clock() -> Arc<HlcClock> {
        HlcClock::new(0)
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn empty_fact_ids_returns_empty_no_write() {
        let ts = Arc::new(TestStorage::default());
        let storage: Arc<dyn StoragePort> = ts.clone();
        let clock = test_clock();
        let scope = dev_scope();
        let result = apply_reflect_invalidate(&storage, &scope, &clock, None, &[]).await.unwrap();
        assert!(result.is_empty(), "empty input yields empty output");
        assert_eq!(ts.write_count(), 0, "no atomic_write for empty input");
    }

    #[tokio::test]
    async fn stamps_live_row_single_atomic_write() {
        let ts = Arc::new(TestStorage::default());
        let storage: Arc<dyn StoragePort> = ts.clone();
        let clock = test_clock();
        let scope = dev_scope();

        let ulid = Ulid::new();
        let (key, bt, payload) = make_fact_row(ulid, false);
        ts.seed_row(key.clone(), bt, payload);

        let result =
            apply_reflect_invalidate(&storage, &scope, &clock, None, &[ulid]).await.unwrap();
        assert_eq!(result, vec![ulid], "stamped ulid returned");

        // D-11: exactly one atomic_write.
        assert_eq!(ts.write_count(), 1, "exactly one atomic_write");

        // Verify bt.sys.1 is set in the persisted bytes.
        let batch = ts.last_batch();
        assert_eq!(batch.len(), 1);
        let WriteOp::KvPut { key: _, value } = &batch[0] else { panic!("expected KvPut") };
        let v: serde_json::Value = serde_json::from_slice(value).unwrap();
        assert!(
            !v["bt"]["sys"][1].is_null(),
            "bt.sys[1] must be non-null after invalidation; got: {}",
            v["bt"]
        );
    }

    #[tokio::test]
    async fn skips_already_invalid_row_no_write() {
        let ts = Arc::new(TestStorage::default());
        let storage: Arc<dyn StoragePort> = ts.clone();
        let clock = test_clock();
        let scope = dev_scope();

        let ulid = Ulid::new();
        let (key, bt, payload) = make_fact_row(ulid, true /* already invalid */);
        ts.seed_row(key, bt, payload);

        let result =
            apply_reflect_invalidate(&storage, &scope, &clock, None, &[ulid]).await.unwrap();
        assert!(result.is_empty(), "already-invalid row must be skipped");
        assert_eq!(ts.write_count(), 0, "no write when all rows already invalid");
    }

    #[tokio::test]
    async fn skips_missing_row_continues_others() {
        let ts = Arc::new(TestStorage::default());
        let storage: Arc<dyn StoragePort> = ts.clone();
        let clock = test_clock();
        let scope = dev_scope();

        let missing = Ulid::new();
        let live = Ulid::new();
        let (key, bt, payload) = make_fact_row(live, false);
        ts.seed_row(key, bt, payload);

        let result = apply_reflect_invalidate(&storage, &scope, &clock, None, &[missing, live])
            .await
            .unwrap();
        assert_eq!(result, vec![live], "only live row returned");
        assert_eq!(ts.write_count(), 1, "one write for the live row");
    }

    #[tokio::test]
    async fn batch_two_live_rows_single_atomic_write() {
        let ts = Arc::new(TestStorage::default());
        let storage: Arc<dyn StoragePort> = ts.clone();
        let clock = test_clock();
        let scope = dev_scope();

        let u1 = Ulid::new();
        let u2 = Ulid::new();
        for ulid in [u1, u2] {
            let (key, bt, payload) = make_fact_row(ulid, false);
            ts.seed_row(key, bt, payload);
        }

        let result =
            apply_reflect_invalidate(&storage, &scope, &clock, None, &[u1, u2]).await.unwrap();
        assert_eq!(result.len(), 2, "both rows stamped");
        assert_eq!(ts.write_count(), 1, "still exactly one atomic_write for a 2-ulid batch");
        assert_eq!(ts.last_batch().len(), 2);
    }

    #[tokio::test]
    async fn idempotent_second_call_produces_no_second_write() {
        let ts = Arc::new(TestStorage::default());
        let storage: Arc<dyn StoragePort> = ts.clone();
        let clock = test_clock();
        let scope = dev_scope();

        let ulid = Ulid::new();
        let (key, bt, payload) = make_fact_row(ulid, false);
        ts.seed_row(key, bt, payload);

        // First call stamps the row.
        let r1 = apply_reflect_invalidate(&storage, &scope, &clock, None, &[ulid]).await.unwrap();
        assert_eq!(r1, vec![ulid]);
        assert_eq!(ts.write_count(), 1);

        // Second call: the row is now already-invalid (atomic_write updated the in-memory map).
        let r2 = apply_reflect_invalidate(&storage, &scope, &clock, None, &[ulid]).await.unwrap();
        assert!(r2.is_empty(), "idempotent: already-invalid row skipped on second call");
        assert_eq!(ts.write_count(), 1, "no second atomic_write");
    }
}
