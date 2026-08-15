//! `BoostProvider` — read-time activation-ledger prior for the post-hydrate
//! boost pass (ADD task activation-ledger).
//!
//! Generalizes the Phase 14.2 `with_boost_cache` LRU seam: instead of an
//! ephemeral per-handle in-memory cache populated only by `end_turn`, a
//! `BoostProvider` reads a PERSISTENT per-memory activation summary (the
//! engram-soul-loop activation ledger, `lunaris_core::activation`) and turns
//! it into an additive score prior — reinforcement survives process restarts
//! and cross-session recall.
//!
//! `RetrievalBuilder::with_boost_provider` is a NEW additive seam. The
//! existing `with_boost_cache` LRU seam is BYTE-IDENTICAL and untouched;
//! the two compose (provider prior applies first, LRU delta applies after —
//! see `builder.rs::execute`). Default `Lunaris::recall()` never wires a
//! provider in this task — opt-in only, so the sub-25ms core recall
//! contract cannot regress for existing callers.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::StreamExt;
use lunaris_core::activation::{self, ActivationRecord};
use lunaris_core::keyspace::activation_key;
use lunaris_core::{HlcClock, Scope, StoragePort};
use ulid::Ulid;

/// Concurrent in-flight point reads per `priors()` call. Hit sets are small
/// (recall k ≤ ~30); 16 keeps wall time ≈ one round trip without hammering
/// the backend connection pool.
const READ_CONCURRENCY: usize = 16;

/// Read-time boost-prior source.
///
/// `priors()` MUST be bounded by the `ids` slice — one batched pass whose
/// storage cost is proportional to the HIT SET, never to the scope's total
/// ledger size (frozen §1 Must, wording amended at review: "one round
/// trip" → "one bounded batch"; a full-prefix scan honors the former and
/// violates the recall budget as the corpus ages). Corrupt or missing
/// ledger entries are simply omitted from the returned map — a malformed
/// record never fails recall (Reject scenario: corrupt record never fails
/// recall).
#[async_trait]
pub trait BoostProvider: Send + Sync {
    async fn priors(&self, scope: &Scope, ids: &[Ulid]) -> HashMap<Ulid, f32>;
}

/// Production `BoostProvider` backed by the persistent activation ledger.
///
/// `StoragePort` has no native point-MGET primitive, so `priors()` issues
/// bounded-concurrency `read_as_of` POINT reads — one per distinct hit id,
/// at most `READ_CONCURRENCY` in flight — instead of scanning the whole
/// `lunaris:{scope}:activation:` prefix. A prefix scan is one round trip
/// today but reads EVERY ledger row in the scope on EVERY recall, so its
/// cost grows with corpus age — exactly the workload this ledger is built
/// for. Point reads keep the cost proportional to the hit set (k ≤ ~30)
/// forever. Each row's activation is recomputed at read time (Anderson
/// 1996 / Petrov 2006, [`activation::DEFAULT_DECAY`]) and converted to a
/// capped prior via [`activation::boost_prior`]. Missing rows are omitted;
/// malformed rows are skipped with `tracing::warn!` — a corrupt ledger
/// entry never fails recall.
///
/// The provider owns a private [`HlcClock`] for the snapshot timestamp; a
/// write landing in the same millisecond as the read tick may be invisible
/// to that one recall, which is harmless for a rank prior (next recall
/// sees it).
pub struct LedgerBoostProvider {
    storage: Arc<dyn StoragePort>,
    clock: Arc<HlcClock>,
}

impl LedgerBoostProvider {
    pub fn new(storage: Arc<dyn StoragePort>) -> Self {
        Self { storage, clock: HlcClock::new(0) }
    }
}

#[async_trait]
impl BoostProvider for LedgerBoostProvider {
    async fn priors(&self, scope: &Scope, ids: &[Ulid]) -> HashMap<Ulid, f32> {
        let mut out = HashMap::new();
        if ids.is_empty() {
            return out;
        }
        // Dedupe defensively — duplicate hit ids would double-read.
        let distinct: Vec<Ulid> = {
            let mut seen = HashSet::new();
            ids.iter().copied().filter(|id| seen.insert(*id)).collect()
        };
        let read_at = self.clock.tick();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

        let rows: Vec<_> = futures::stream::iter(distinct.into_iter().map(|id| {
            let key = activation_key(scope, id);
            async move {
                match self.storage.read_as_of(scope, &key, read_at).await {
                    Ok(Some(row)) => (id, Some(row.value)),
                    Ok(None) => (id, None),
                    Err(e) => {
                        tracing::warn!(
                            err = %e,
                            %id,
                            "activation_ledger_boost_provider_read_failed"
                        );
                        (id, None)
                    }
                }
            }
        }))
        .buffer_unordered(READ_CONCURRENCY)
        .collect()
        .await;

        for (id, value) in rows {
            let Some(value) = value else { continue };
            let record: ActivationRecord = match serde_json::from_slice(&value) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(err = %e, %id, "activation_ledger_corrupt_record_skipped");
                    continue;
                }
            };
            // engram-soul-loop task 8b — `memory.distill` archives a source
            // record's `archived_at` (activation drop, NOT a tombstone: the
            // episode itself stays recall-hydratable). An archived record
            // contributes NO boost at all — omitted from the map entirely,
            // same treatment as a missing/corrupt row.
            if record.is_archived() {
                continue;
            }
            let a = record.activation(now, activation::DEFAULT_DECAY);
            out.insert(id, activation::boost_prior(a));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::activation::{Grain, RefSignal, Strength};
    use lunaris_core::storage::types::{
        CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
    };
    use lunaris_core::{BiTemporal, Hlc, StorageCapabilities, StorageError};

    /// Minimal `StoragePort` double — only `read_as_of` is meaningful (the
    /// only method `LedgerBoostProvider::priors` calls). Every other method
    /// panics if reached, proving the provider never touches them.
    #[derive(Default)]
    struct FakeLedgerStorage {
        rows: parking_lot::Mutex<HashMap<Vec<u8>, bytes::Bytes>>,
    }

    #[async_trait]
    impl StoragePort for FakeLedgerStorage {
        async fn atomic_write(
            &self,
            _scope: &Scope,
            _ops: &[WriteOp],
        ) -> Result<Lsn, StorageError> {
            panic!("priors() must never write");
        }
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
            panic!("priors() must never vector_search");
        }
        async fn graph_traverse(
            &self,
            _scope: &Scope,
            _q: &CypherQuery,
            _as_of: Option<Hlc>,
        ) -> Result<GraphResult, StorageError> {
            panic!("priors() must never graph_traverse");
        }
        async fn scan_range(
            &self,
            _scope: &Scope,
            _prefix: &[u8],
            _as_of: Option<Hlc>,
        ) -> Result<
            futures::stream::BoxStream<'_, Result<(bytes::Bytes, bytes::Bytes), StorageError>>,
            StorageError,
        > {
            panic!("priors() must never scan_range — point reads only (bounded by hit set)");
        }
        async fn read_as_of(
            &self,
            _scope: &Scope,
            key: &[u8],
            _as_of: Hlc,
        ) -> Result<Option<Row<bytes::Bytes>>, StorageError> {
            Ok(self.rows.lock().get(key).cloned().map(|value| Row {
                key: key.to_vec(),
                value,
                bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
            }))
        }
        async fn publish(
            &self,
            _scope: &Scope,
            _topic: &str,
            _partition: u16,
            _payload: bytes::Bytes,
        ) -> Result<u64, StorageError> {
            panic!("priors() must never publish");
        }
        async fn subscribe(
            &self,
            _scope: &Scope,
            _group: &str,
            _topic: &str,
            _partition: u16,
        ) -> Result<futures::stream::BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError>
        {
            panic!("priors() must never subscribe");
        }
        fn capabilities(&self) -> StorageCapabilities {
            panic!("priors() must never read capabilities");
        }
    }

    /// §6 VERIFY: "archive proven by a recall-boost-suppression assertion
    /// (not a stub)". This calls the REAL `LedgerBoostProvider::priors` —
    /// only the storage backing is faked (mirrors the codebase-wide
    /// `RecordingStorage`/`LedgerTestStorage` convention) — against two REAL
    /// `ActivationRecord` rows, one archived and one live. Archived must
    /// contribute NO entry (0 boost); live must still boost.
    #[tokio::test]
    async fn archived_record_contributes_zero_boost_live_record_still_boosts() {
        let scope = Scope::new("test.boost-provider-archived").unwrap();
        let id_archived = Ulid::new();
        let id_live = Ulid::new();
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

        let mut archived_rec = ActivationRecord::default();
        archived_rec.apply(
            &RefSignal { id: id_archived, grain: Grain::Turn, strength: Strength::Strong },
            now,
        );
        archived_rec.archived_at = Some(now);
        assert!(archived_rec.is_archived());

        let mut live_rec = ActivationRecord::default();
        live_rec
            .apply(&RefSignal { id: id_live, grain: Grain::Turn, strength: Strength::Strong }, now);
        assert!(!live_rec.is_archived());

        let storage = Arc::new(FakeLedgerStorage::default());
        storage.rows.lock().insert(
            activation_key(&scope, id_archived),
            serde_json::to_vec(&archived_rec).unwrap().into(),
        );
        storage
            .rows
            .lock()
            .insert(activation_key(&scope, id_live), serde_json::to_vec(&live_rec).unwrap().into());

        let provider = LedgerBoostProvider::new(storage as Arc<dyn StoragePort>);
        let priors = provider.priors(&scope, &[id_archived, id_live]).await;

        assert!(
            !priors.contains_key(&id_archived),
            "archived record must contribute 0 boost (omitted entirely): {priors:?}"
        );
        assert!(
            priors.get(&id_live).copied().unwrap_or(0.0) > 0.0,
            "live record must still boost: {priors:?}"
        );
    }
}
