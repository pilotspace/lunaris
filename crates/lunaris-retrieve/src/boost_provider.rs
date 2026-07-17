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
/// at most [`READ_CONCURRENCY`] in flight — instead of scanning the whole
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
            let a = record.activation(now, activation::DEFAULT_DECAY);
            out.insert(id, activation::boost_prior(a));
        }
        out
    }
}
