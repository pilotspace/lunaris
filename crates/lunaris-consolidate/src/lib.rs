//! lunaris-consolidate — Phase 4 ACT-R consolidator (Plan 04-02).
//!
//! Per blueprint §5.1 the v0 consolidator is **default-OFF**: the umbrella
//! `Lunaris` handle constructs a [`NoopConsolidator`] and the worker thread is
//! NOT spawned until `handle.consolidator_pipeline().enable()` is called
//! (Plan 04-04).
//!
//! Unlike [`lunaris_verify`], this crate has **no LLM backends** per D-15 —
//! community summaries for CONSOL-04 are produced by Phase 3's `Extractor`
//! trait acting as a summarizer and are wired at the umbrella handle layer
//! in Plan 04-04. Keeping this crate pure-data keeps the build fast and the
//! dep tree minimal.
//!
//! ## Surface
//!
//! - [`Consolidator`] trait — dyn-compatible async surface consumed by the
//!   worker loop.
//! - [`NoopConsolidator`] — unconditional no-op, `applies()==false`.
//! - [`ActRScorer`] — Anderson 1996 base-level activation + Petrov 2006 O(1)
//!   incremental approximation per D-13.
//! - [`leiden_pass`] — hand-rolled label-propagation community detection per
//!   D-15 Option A. Rustworkx is explicitly rejected because it carries
//!   unsafe blocks and would violate [`#![forbid(unsafe_code)]`].
//! - [`run_consolidate_worker`] — in-process tokio worker subscribing to
//!   `__lunaris_consolidate__` (D-06 consumer group `lunaris-consolidate-v0`).
//!
//! ## B-1 audit emit shape (D-22 verbatim)
//!
//! When the worker applies a consolidation pass, it emits per-event audit
//! records matching Plan 04-05 `AuditEvent` variants verbatim:
//!
//! - one `AuditEvent::ConsolidatorPromotion { episode_id, fact_id,
//!   activation_score }` per promoted Episode→Fact
//! - one `AuditEvent::ConsolidatorArchive { fact_id, final_activation,
//!   moved_to }` per archived Fact
//!
//! There is NO rolled-up `ConsolidatorReport` audit variant — Plan 04-05's
//! enum does not carry that shape. The [`ConsolidationReport`] returned from
//! [`Consolidator::consolidate`] carries per-event vectors (`promotions:
//! Vec<PromotionEvent>`, `archives: Vec<ArchiveEvent>`) so the worker can
//! iterate and publish one audit message per event.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use lunaris_core::{LunarisError, StoragePort};

pub mod act_r;
pub mod leiden;
pub mod noop;
pub mod types;
pub mod worker;

pub use act_r::ActRScorer;
pub use leiden::{CommunityAssignment, GraphSnapshot, leiden_pass};
pub use noop::NoopConsolidator;
pub use types::{
    ArchiveEvent, CommunityId, ConsolidateEvent, ConsolidationReport, ConsolidatorConfig, FactId,
    IndexKind, PromotionEvent, ReferenceTime,
};
pub use worker::{CONSOLIDATE_CONSUMER_GROUP, CONSOLIDATE_TOPIC, run_consolidate_worker};

/// Object-safe async consolidator.
///
/// Per blueprint §5.1 + D-02 the v0 default implementation is
/// [`NoopConsolidator`]; alternative implementations (e.g., the default
/// ACT-R-backed consolidator wired by Plan 04-04) plug in via
/// `ConsolidatorPipelineHandle::set_consolidator`.
///
/// `Arc<dyn Consolidator>` is constructible (proven by the compile-time
/// `consolidator_is_dyn_compat` test), so the umbrella handle can carry the
/// installed consolidator without compile-time monomorphization.
#[async_trait]
pub trait Consolidator: Send + Sync + 'static {
    /// Run one consolidation pass over a batch of debounced
    /// [`ConsolidateEvent`]s pulled off the `__lunaris_consolidate__` queue.
    ///
    /// Returns a [`ConsolidationReport`] carrying per-promotion + per-archive
    /// events (B-1 — D-22 verbatim shape), surfaced 1:1 to the audit log via
    /// `AuditEvent::ConsolidatorPromotion` + `AuditEvent::ConsolidatorArchive`
    /// by the worker.
    async fn consolidate(
        &self,
        storage: Arc<dyn StoragePort>,
        events: &[ConsolidateEvent],
    ) -> Result<ConsolidationReport, LunarisError>;

    /// Returns `true` when this consolidator produces real work; `false` for
    /// [`NoopConsolidator`] so the worker can short-circuit before iterating
    /// the event buffer.
    fn applies(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time proof the trait is dyn-compatible (object-safe). If a
    /// future addition (generic method, `Self: Sized` bound) breaks this, the
    /// `Arc<dyn Consolidator>` form on the umbrella handle stops compiling.
    #[test]
    fn consolidator_is_dyn_compat() {
        fn _check<T: Consolidator + ?Sized>() {}
        _check::<dyn Consolidator>();
        let _: Arc<dyn Consolidator> = Arc::new(NoopConsolidator);
    }
}
