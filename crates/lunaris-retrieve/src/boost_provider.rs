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
use lunaris_core::keyspace::activation_prefix;
use lunaris_core::{Scope, StoragePort};
use ulid::Ulid;

/// Read-time boost-prior source.
///
/// `priors()` MUST be a single batched storage read for the whole `ids`
/// slice (frozen §1 Must — the recall p50 budget assumes one round trip,
/// not `k` point reads). Corrupt or missing ledger entries are simply
/// omitted from the returned map — a malformed record never fails recall
/// (Reject scenario: corrupt record never fails recall).
#[async_trait]
pub trait BoostProvider: Send + Sync {
    async fn priors(&self, scope: &Scope, ids: &[Ulid]) -> HashMap<Ulid, f32>;
}

/// Production `BoostProvider` backed by the persistent activation ledger.
///
/// `StoragePort` has no native point-MGET primitive, so `priors()` issues
/// ONE `scan_range` call over the scope's `lunaris:{scope}:activation:`
/// prefix (the one-round-trip primitive available — see ADD task
/// activation-ledger §1 lowest-confidence assumption), decodes every row,
/// recomputes activation at read time (Anderson 1996 / Petrov 2006,
/// [`activation::DEFAULT_DECAY`]), and converts to a capped prior via
/// [`activation::boost_prior`]. Rows outside the requested `ids` set are
/// skipped; malformed rows are skipped with `tracing::warn!` — a corrupt
/// ledger entry never fails recall.
pub struct LedgerBoostProvider {
    storage: Arc<dyn StoragePort>,
}

impl LedgerBoostProvider {
    pub fn new(storage: Arc<dyn StoragePort>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl BoostProvider for LedgerBoostProvider {
    async fn priors(&self, scope: &Scope, ids: &[Ulid]) -> HashMap<Ulid, f32> {
        let mut out = HashMap::new();
        if ids.is_empty() {
            return out;
        }
        let wanted: HashSet<Ulid> = ids.iter().copied().collect();
        let prefix = activation_prefix(scope);

        let mut stream = match self.storage.scan_range(scope, &prefix, None).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(err = %e, "activation_ledger_boost_provider_scan_failed");
                return out;
            }
        };

        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

        while let Some(item) = stream.next().await {
            let (key, value) = match item {
                Ok(kv) => kv,
                Err(e) => {
                    tracing::warn!(err = %e, "activation_ledger_boost_provider_row_read_failed");
                    continue;
                }
            };
            let Some(id) = parse_activation_id(&key) else { continue };
            if !wanted.contains(&id) {
                continue;
            }
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

/// Recover the trailing `{ulid}` segment from an
/// `lunaris:{scope}:activation:{ulid}` key.
fn parse_activation_id(key: &[u8]) -> Option<Ulid> {
    let s = std::str::from_utf8(key).ok()?;
    let idx = s.rfind(':')?;
    Ulid::from_string(&s[idx + 1..]).ok()
}
