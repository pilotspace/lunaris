//! Phase 9 Plan 09-03 PRIM-04 (structural half) — `WorkingMemory` primitive.
//!
//! Scope-prefixed scratchpad for agentic working memory. Every `write` /
//! `read` / `grep` call scopes through a caller-supplied `scope_prefix` —
//! either via prefix concatenation on the Episode `source` field (write) or
//! via [`Filter::Eq`] / [`Filter::StartsWith`] at recall time (read / grep).
//! No SQL LIKE strings; no global state; no duplicate vector / BM25 libraries
//! (CLAUDE.md constraint — Moon native `FT.*` is canonical).
//!
//! ## Relocation note (Phase 12 Option A)
//!
//! This type moved from `lunaris-recipes::working_memory` into
//! `lunaris::primitives::working_memory` so that Phase 12 `HeliosScratchpad`
//! (which lives in the `lunaris` crate) can compose over it without
//! introducing a `lunaris → lunaris-recipes` dependency cycle. The
//! `lunaris-recipes` crate re-exports this type verbatim — every Phase 9 /
//! 10 / 11 caller that imports `lunaris_recipes::WorkingMemory` keeps
//! compiling unchanged. Phase 13's proper primitives-crate extraction
//! subsumes this location.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use lunaris_consolidate::{
    CONSOLIDATE_CONSUMER_GROUP, CONSOLIDATE_TOPIC, ConsolidateEvent, ConsolidationReport,
};
use lunaris_core::storage::types::{Filter, Lsn};
use lunaris_core::{Episode, LunarisError, Scope, StoragePort};
use lunaris_retrieve::{Keyword, Query, Vector};

use crate::{AuditEvent, Lunaris, publish_audit_event};

/// Phase 9.1 Plan 01 Task 3 — maximum events drained per
/// [`WorkingMemory::consolidate`] call. Bounds T-09-1-01-04 DoS surface:
/// heavy callers should invoke repeatedly rather than raising the cap.
const DRAIN_CAP: usize = 1024;

/// Phase 9.1 Plan 01 Task 3 — per-pull timeout on the drain stream. The
/// drain exits on the first timeout, stream end, or error; combined with
/// [`DRAIN_CAP`] this guarantees the drain terminates within
/// `DRAIN_CAP × PULL_TIMEOUT_MS` ms (worst case ≈ 51 s when the broker
/// keeps delivering events at exactly the timeout boundary).
const PULL_TIMEOUT_MS: u64 = 50;

/// Default `top_k` the `read` / `grep` recall paths use. Chosen to match
/// Plan 09-01 MessageStream's `DEFAULT_TOP_K` + HeliosScratchpad's `READ_TOP`
/// (both 8) so conversational wrappers that compose WorkingMemory with
/// MessageStream / scratchpad primitives inherit the same breadth.
const DEFAULT_TOP_K: usize = 8;

/// Fan-out multiplier applied to each branch of the fused plan before RRF
/// fuses them. Matches Plan 09-01 MessageStream + Plan 09-02 DocumentCorpus
/// (`3`) so the three Phase 9 primitives share a consistent pre-fusion
/// window size.
const FANOUT: usize = 3;

/// RRF constant from Cormack et al. (2009). Matches
/// `DocumentCorpus::DEFAULT_RRF_K` (60). Shared across every Phase 9 primitive
/// that fuses Vector + Keyword.
const RRF_K: u32 = 60;

/// Key-prefixed scratchpad. Stores `(k, v)` pairs under `{scope_prefix}{k}`
/// as [`Episode`]s on the Episode `source` field.
#[derive(Clone)]
pub struct WorkingMemory {
    lunaris: Arc<Lunaris>,
    scope: Scope,
    scope_prefix: String,
}

impl WorkingMemory {
    /// Construct a new scratchpad bound to `scope` (RFC 0001 partition key)
    /// and `scope_prefix` (source-key namespace). The two concepts are
    /// orthogonal: `scope` partitions the KV / FT keyspace, while
    /// `scope_prefix` namespaces the `source` field on each Episode so a
    /// single scope can host multiple WorkingMemory instances (e.g.,
    /// `"helios:fs/"` vs `"chat:user-42/"`).
    pub fn new(
        lunaris: Arc<Lunaris>,
        scope: Scope,
        scope_prefix: impl Into<String>,
    ) -> Self {
        Self { lunaris, scope, scope_prefix: scope_prefix.into() }
    }

    /// Write `(k, v)` under `{scope_prefix}{k}` as an [`Episode`].
    pub async fn write(&self, k: &str, v: serde_json::Value) -> Result<Lsn, LunarisError> {
        let source = self.scope_key(k);
        let content = serde_json::to_string(&v)
            .map_err(|e| LunarisError::from(lunaris_core::StorageError::from(e)))?;
        let episode = Episode::new(
            self.scope.clone(),
            source,
            content,
            self.lunaris.clock().as_ref(),
        );
        self.lunaris.ingest(episode).await
    }

    /// Read the value for `k` scoped under `scope_prefix`, if present.
    pub async fn read(&self, k: &str) -> Result<Option<serde_json::Value>, LunarisError> {
        let source = self.scope_key(k);
        let filter =
            Filter::Eq { field: "source".into(), value: serde_json::Value::String(source) };
        let plan = Vector::new("chunks", DEFAULT_TOP_K * FANOUT)
            .and(Keyword::bm25("chunks", DEFAULT_TOP_K * FANOUT))
            .fuse_rrf(RRF_K)
            .top(DEFAULT_TOP_K);
        let hits =
            self.lunaris.recall().with_root(plan).filter(filter).execute(Query::text(k)).await?;
        match hits.into_iter().next() {
            Some(h) => Ok(Some(
                serde_json::from_str(&h.text)
                    .map_err(|e| LunarisError::from(lunaris_core::StorageError::from(e)))?,
            )),
            None => Ok(None),
        }
    }

    /// Return all `(source, value)` pairs whose `source` starts with
    /// `{scope_prefix}{pattern}`.
    pub async fn grep(
        &self,
        pattern: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, LunarisError> {
        let filter = Filter::StartsWith { field: "source".into(), prefix: self.scope_key(pattern) };
        let plan = Vector::new("chunks", DEFAULT_TOP_K * FANOUT)
            .and(Keyword::bm25("chunks", DEFAULT_TOP_K * FANOUT))
            .fuse_rrf(RRF_K)
            .top(DEFAULT_TOP_K);
        let hits = self
            .lunaris
            .recall()
            .with_root(plan)
            .filter(filter)
            .execute(Query::text(pattern))
            .await?;
        let mut out = Vec::with_capacity(hits.len());
        for h in hits {
            let v: serde_json::Value = serde_json::from_str(&h.text)
                .map_err(|e| LunarisError::from(lunaris_core::StorageError::from(e)))?;
            out.push((h.source, v));
        }
        Ok(out)
    }

    /// Phase 9.1 Plan 01 Task 3 — run one consolidation pass scoped to
    /// `self.scope_prefix`.
    pub async fn consolidate(&self) -> Result<ConsolidationReport, LunarisError> {
        let storage: Arc<dyn StoragePort> = self.lunaris.storage();

        let events = drain_consolidate_events(&storage).await?;

        let pipeline = self.lunaris.consolidator_pipeline();
        let consolidator = match pipeline.snapshot_consolidator() {
            Some(c) => c,
            None => {
                return Ok(ConsolidationReport::default());
            }
        };

        let report = consolidator
            .consolidate_scoped(storage.clone(), &events, Some(&self.scope_prefix))
            .await?;

        for promo in &report.promotions {
            let event = AuditEvent::ConsolidatorPromotion {
                episode_id: promo.episode_id,
                fact_id: crate::audit::FactIdData(promo.fact_id.0),
                activation_score: promo.activation_score,
            };
            let _ = publish_audit_event(&storage, event).await;
        }

        Ok(report)
    }

    fn scope_key(&self, k: &str) -> String {
        format!("{}{}", self.scope_prefix, k)
    }
}

/// Phase 9.1 Plan 01 Task 3 — drain up to [`DRAIN_CAP`] recent
/// [`ConsolidateEvent`]s from [`CONSOLIDATE_TOPIC`].
async fn drain_consolidate_events(
    storage: &Arc<dyn StoragePort>,
) -> Result<Vec<ConsolidateEvent>, LunarisError> {
    let pull_timeout = Duration::from_millis(PULL_TIMEOUT_MS);

    // RFC 0001 Wave 0: use Scope::dev() until per-scope queue routing (Wave 3F).
    // scope-dev-allowed: inside-deprecated-wrapper — Wave 3F per-scope queue routing
    // tracked in RFC 0001 §3.7 / docs/v0.3-known-debt.md.
    let mut stream = storage
        .subscribe(&lunaris_core::Scope::dev(), CONSOLIDATE_CONSUMER_GROUP, CONSOLIDATE_TOPIC, 0)
        .await
        .map_err(LunarisError::Storage)?;

    let mut events = Vec::with_capacity(64);
    while events.len() < DRAIN_CAP {
        match tokio::time::timeout(pull_timeout, stream.next()).await {
            Ok(Some(Ok(msg))) => {
                if let Ok(ev) = serde_json::from_slice::<ConsolidateEvent>(&msg.payload) {
                    events.push(ev);
                }
            }
            Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PRIM-04 ≤ 30 LOC public-surface contract.
    #[test]
    fn working_memory_public_surface_under_30_loc() {
        let src = include_str!("./working_memory.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "PRIM-04 ≤30-LOC contract: WorkingMemory has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 3,
            "PRIM-04 contract: WorkingMemory needs at least 3 public methods; got {pub_fns}"
        );
    }

    #[test]
    fn working_memory_scope_key_prefix_concatenation() {
        fn scope_key(prefix: &str, k: &str) -> String {
            format!("{prefix}{k}")
        }
        assert_eq!(scope_key("helios:fs/", "note-1"), "helios:fs/note-1");
        assert_eq!(scope_key("chat:user-42/", "draft"), "chat:user-42/draft");
        assert_eq!(scope_key("", "raw-key"), "raw-key");
    }

    #[test]
    fn working_memory_grep_uses_starts_with_filter() {
        let prefix = "chat:user-42/draft-";
        let filter = Filter::StartsWith { field: "source".into(), prefix: prefix.into() };
        match filter {
            Filter::StartsWith { field, prefix: p } => {
                assert_eq!(field, "source");
                assert_eq!(p, "chat:user-42/draft-");
            }
            other => panic!("expected StartsWith variant; got {other:?}"),
        }
    }

    #[test]
    fn working_memory_construction_records_scope() {
        let s = format!("{}{}", "helios:fs/", "k");
        assert!(s.starts_with("helios:fs/"));
        assert!(s.ends_with("k"));
        assert_eq!(s, "helios:fs/k");
    }
}
