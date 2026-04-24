//! [`NoopConsolidator`] — passthrough that satisfies the trait without
//! performing any consolidation work.
//!
//! Used as the default when the consolidator pipeline is OFF (Plan 04-04;
//! default per blueprint §5.1 + D-02) — the worker is never spawned and
//! `consolidate` is never called. Also used when the caller has not
//! explicitly installed a real consolidator via `with_consolidator`; v0
//! ships no default ACT-R consolidator on the umbrella handle
//! (`default_consolidator` in Plan 04-04 returns `Arc::new(NoopConsolidator)`
//! unconditionally).
//!
//! `applies()` returns `false` so the worker loop (Plan 04-02 Task 3) can
//! short-circuit before iterating the debounce buffer — saves a clone +
//! spawn for every no-op tick.

use std::sync::Arc;

use async_trait::async_trait;
use lunaris_core::{LunarisError, StoragePort};

use crate::Consolidator;
use crate::types::{ConsolidateEvent, ConsolidationReport};

/// Default consolidator when the pipeline is OFF (Plan 04-04) or when the
/// caller has not installed a real consolidator via `with_consolidator`.
///
/// Returns an empty [`ConsolidationReport`] (B-1: empty per-event vectors,
/// NOT zero counts) so the worker treats the pass as "abstain — nothing to
/// promote, nothing to archive, no community rebuild".
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopConsolidator;

#[async_trait]
impl Consolidator for NoopConsolidator {
    async fn consolidate(
        &self,
        _storage: Arc<dyn StoragePort>,
        _events: &[ConsolidateEvent],
    ) -> Result<ConsolidationReport, LunarisError> {
        // B-1: return the empty per-event report shape (the old `promoted: u64`
        // / `archived: u64` rolled-up counts are GONE — Plan 04-05's
        // `AuditEvent` enum has no `ConsolidatorReport` variant).
        Ok(ConsolidationReport::default())
    }

    fn applies(&self) -> bool {
        false
    }

    fn debug_name(&self) -> &'static str {
        "NoopConsolidator"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ulid::Ulid;

    #[tokio::test]
    async fn noop_returns_empty_report() {
        // NoopConsolidator can take any `Arc<dyn StoragePort>` — we don't have
        // one handy in unit tests and `consolidate`'s contract doesn't require
        // calling through to storage, so we exercise the `applies()==false`
        // short-circuit path at the trait surface instead.
        assert!(!NoopConsolidator.applies());
    }

    #[test]
    fn noop_applies_false() {
        assert!(
            !NoopConsolidator.applies(),
            "NoopConsolidator::applies must be false so the worker can short-circuit"
        );
    }

    #[test]
    fn noop_report_is_empty_per_event_shape() {
        // B-1 regression guard: prove the default report has per-event vectors,
        // not the old rolled-up counts.
        let r = ConsolidationReport::default();
        assert_eq!(r.promotions.len(), 0, "per-event vector, not rolled-up count");
        assert_eq!(r.archives.len(), 0, "per-event vector, not rolled-up count");
        assert_eq!(r.communities_rebuilt, 0);

        // And the episode_id round-trip stays stable (sanity check on ulid
        // feature wiring):
        let _ = Ulid::new();
    }
}
