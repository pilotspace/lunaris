//! `LedgerReferenceSource` — ADD task activation-ledger, "ACT-R worker reads
//! ledger references."
//!
//! Feeds [`crate::ActRConsolidator`] reference signals from the persistent
//! per-memory activation ledger (`lunaris_core::activation`) instead of
//! `ConsolidateEvent` write-frequency signals, so promote/archive decisions
//! measure USE (recall injections / citations / successful tool calls) —
//! not how often a memory was WRITTEN.

use std::sync::Arc;

use lunaris_core::activation::ActivationRecord;
use lunaris_core::keyspace::activation_prefix;
use lunaris_core::{LunarisError, Scope, StoragePort};
use ulid::Ulid;

/// Background-only reference source: a FULL-SCOPE scan of every activation
/// record under `lunaris:{scope}:activation:`.
///
/// Per the frozen §3 CONTRACT Access note, a full-scope scan is reserved for
/// the ACT-R tick (background maintenance) — the recall hot path uses the
/// BATCHED [`lunaris_retrieve::LedgerBoostProvider`] read instead, which
/// only asks for the ids in the current hit set.
pub struct LedgerReferenceSource {
    storage: Arc<dyn StoragePort>,
}

impl LedgerReferenceSource {
    pub fn new(storage: Arc<dyn StoragePort>) -> Self {
        Self { storage }
    }

    /// Scan every activation record in `scope`. Malformed rows are skipped
    /// with `tracing::warn!` — a corrupt ledger entry never fails the tick.
    pub async fn scan(&self, scope: &Scope) -> Result<Vec<(Ulid, ActivationRecord)>, LunarisError> {
        use futures::StreamExt;

        let prefix = activation_prefix(scope);
        let mut stream =
            self.storage.scan_range(scope, &prefix, None).await.map_err(LunarisError::Storage)?;

        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            let (key, value) = match item {
                Ok(kv) => kv,
                Err(e) => {
                    tracing::warn!(err = %e, "ledger_reference_source_row_read_failed");
                    continue;
                }
            };
            let Some(id) = parse_activation_id(&key) else { continue };
            match serde_json::from_slice::<ActivationRecord>(&value) {
                Ok(record) => out.push((id, record)),
                Err(e) => {
                    tracing::warn!(err = %e, %id, "ledger_reference_source_corrupt_record_skipped");
                }
            }
        }
        Ok(out)
    }
}

/// Recover the trailing `{ulid}` segment from an
/// `lunaris:{scope}:activation:{ulid}` key.
fn parse_activation_id(key: &[u8]) -> Option<Ulid> {
    let s = std::str::from_utf8(key).ok()?;
    let idx = s.rfind(':')?;
    Ulid::from_string(&s[idx + 1..]).ok()
}
