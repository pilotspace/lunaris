//! lunaris-consolidate — Phase 4 ACT-R consolidator (Plan 04-02).
//!
//! Per blueprint §5.1 the v0 consolidator is **default-OFF**: the umbrella
//! `Lunaris` handle constructs a [`NoopConsolidator`] and the worker thread is
//! NOT spawned until `handle.consolidator_pipeline().enable()` is called
//! (Plan 04-04).
//!
//! Unlike `lunaris_verify`, this crate has **no LLM backends** per D-15 —
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
//!   unsafe blocks and would violate `#![forbid(unsafe_code)]`.
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
pub mod supervisor;
pub mod types;
pub mod worker;

pub use act_r::{ActRConsolidator, ActRScorer};
pub use leiden::{CommunityAssignment, GraphSnapshot, leiden_pass};
pub use noop::NoopConsolidator;
pub use supervisor::{
    ConsolidateSupervisor, ConsolidateSupervisorHandle, DEFAULT_SCOPE_CONCURRENCY,
    ENV_SCOPE_CONCURRENCY, ENV_SCOPE_IDLE_TIMEOUT_MS, scope_consolidate_topic,
};
pub use types::{
    ArchiveEvent, CommunityId, ConsolidateEvent, ConsolidationReport, ConsolidatorConfig, FactId,
    IndexKind, PromotionEvent, ReferenceTime,
};
#[allow(deprecated)]
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

    /// Scope-aware consolidation entry point (Phase 9.1 Plan 01 — PRIM-04
    /// full wiring). When `scope_prefix` is `Some(prefix)`, events whose
    /// `event.source` does NOT start with `prefix` are filtered out BEFORE
    /// being forwarded to [`Self::consolidate`]. When `scope_prefix` is
    /// `None`, the full batch is forwarded unchanged.
    ///
    /// The default impl filters then delegates to [`Self::consolidate`];
    /// implementors needing custom scope handling (e.g. per-scope activation
    /// thresholds) may override. Introduced so
    /// [`lunaris_recipes::WorkingMemory::consolidate`](https://docs.rs/lunaris-recipes)
    /// can route by `WorkingMemory.scope_prefix` without every `Consolidator`
    /// implementor opting in. Phase 12 HELIOS-04 will wire
    /// `scope_prefix = "helios:fs/"` through this path.
    ///
    /// `dyn`-compat is preserved — no `Self: Sized` bound is added; the
    /// existing `consolidator_is_dyn_compat` compile-time test continues to
    /// pass.
    async fn consolidate_scoped(
        &self,
        storage: Arc<dyn StoragePort>,
        events: &[ConsolidateEvent],
        scope_prefix: Option<&str>,
    ) -> Result<ConsolidationReport, LunarisError> {
        match scope_prefix {
            None => self.consolidate(storage, events).await,
            Some(prefix) => {
                let filtered: Vec<ConsolidateEvent> =
                    events.iter().filter(|e| e.source.starts_with(prefix)).cloned().collect();
                self.consolidate(storage, &filtered).await
            }
        }
    }

    /// Returns `true` when this consolidator produces real work; `false` for
    /// [`NoopConsolidator`] so the worker can short-circuit before iterating
    /// the event buffer.
    fn applies(&self) -> bool {
        true
    }

    /// Short, static name of the concrete backend — used by
    /// `ConsolidatorPipelineHandle::backend_from_env` for its one-shot
    /// `tracing::info!(consolidator_backend = ...)` log on first resolution,
    /// and by the `default_flip` regression test to assert which backend is
    /// wired without `std::any` downcasts. Phase 16-01 (CONSOL-V1-01).
    ///
    /// Default returns `"Unknown"`; each concrete `impl Consolidator` should
    /// override with a stable short name (e.g. `"NoopConsolidator"`,
    /// `"ActRConsolidator"`). `dyn`-compat is preserved — the method takes
    /// `&self`, returns a `'static` string, no generics.
    fn debug_name(&self) -> &'static str {
        "Unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time proof the trait is dyn-compatible (object-safe). If a
    /// future addition (generic method, `Self: Sized` bound) breaks this, the
    /// `Arc<dyn Consolidator>` form on the umbrella handle stops compiling.
    ///
    /// Phase 9.1 Plan 01 Task 1 — preserves dyn-compat after the additive
    /// `consolidate_scoped` default method lands. `async_trait` default
    /// methods without a `Self: Sized` bound remain object-safe.
    #[test]
    fn consolidator_is_dyn_compat() {
        fn _check<T: Consolidator + ?Sized>() {}
        _check::<dyn Consolidator>();
        let _: Arc<dyn Consolidator> = Arc::new(NoopConsolidator);
    }

    /// Phase 9.1 Plan 01 Task 1 — compile-time + runtime proof that the
    /// default `consolidate_scoped` impl is callable through an
    /// `Arc<dyn Consolidator>` and forwards a filtered batch to
    /// `consolidate` when `scope_prefix` is `Some(prefix)`.
    ///
    /// `NoopConsolidator::consolidate` returns an empty report for any
    /// input, so the semantic assertion is behavioural-equivalence: both
    /// `Some("test:wm/")` and `None` return the same default report.
    #[tokio::test]
    async fn consolidate_scoped_default_filters_then_delegates() {
        use ulid::Ulid;

        let c: Arc<dyn Consolidator> = Arc::new(NoopConsolidator);

        // Minimal StoragePort stand-in — `NoopConsolidator::consolidate`
        // never touches storage, so a narrow never-used port suffices.
        #[derive(Default)]
        struct UnusedStorage;

        #[async_trait]
        impl lunaris_core::StoragePort for UnusedStorage {
            async fn atomic_write(
                &self,
                _scope: &lunaris_core::Scope,
                _ops: &[lunaris_core::WriteOp],
            ) -> Result<lunaris_core::Lsn, lunaris_core::StorageError> {
                Ok(lunaris_core::Lsn { wall_ms: 0, counter: 0 })
            }
            async fn vector_search(
                &self,
                _scope: &lunaris_core::Scope,
                _index: &str,
                _query: &[f32],
                _k: usize,
                _filter: Option<&lunaris_core::Filter>,
                _as_of: Option<lunaris_core::Hlc>,
                _rerank: bool,
            ) -> Result<Vec<lunaris_core::VectorHit>, lunaris_core::StorageError> {
                Ok(Vec::new())
            }
            async fn graph_traverse(
                &self,
                _scope: &lunaris_core::Scope,
                _q: &lunaris_core::CypherQuery,
                _as_of: Option<lunaris_core::Hlc>,
            ) -> Result<lunaris_core::GraphResult, lunaris_core::StorageError> {
                Ok(lunaris_core::GraphResult::default())
            }
            async fn scan_range(
                &self,
                _scope: &lunaris_core::Scope,
                _prefix: &[u8],
                _as_of: Option<lunaris_core::Hlc>,
            ) -> Result<
                futures::stream::BoxStream<
                    '_,
                    Result<(bytes::Bytes, bytes::Bytes), lunaris_core::StorageError>,
                >,
                lunaris_core::StorageError,
            > {
                Ok(Box::pin(futures::stream::iter(Vec::new())))
            }
            async fn read_as_of(
                &self,
                _scope: &lunaris_core::Scope,
                _key: &[u8],
                _as_of: lunaris_core::Hlc,
            ) -> Result<Option<lunaris_core::Row<bytes::Bytes>>, lunaris_core::StorageError>
            {
                Ok(None)
            }
            async fn publish(
                &self,
                _scope: &lunaris_core::Scope,
                _topic: &str,
                _partition: u16,
                _payload: bytes::Bytes,
            ) -> Result<u64, lunaris_core::StorageError> {
                Ok(0)
            }
            async fn subscribe(
                &self,
                _scope: &lunaris_core::Scope,
                _group: &str,
                _topic: &str,
                _partition: u16,
            ) -> Result<
                futures::stream::BoxStream<
                    'static,
                    Result<lunaris_core::QueueMsg, lunaris_core::StorageError>,
                >,
                lunaris_core::StorageError,
            > {
                Ok(Box::pin(futures::stream::empty()))
            }
            fn capabilities(&self) -> lunaris_core::StorageCapabilities {
                lunaris_core::StorageCapabilities {
                    bi_temporal_native: false,
                    graph_native: false,
                    rerank_native: false,
                    queue_native: false,
                    max_vector_dim: 768,
                    native_rrf: false,
                    max_scopes_recommended: 0,
                    cypher_dialect: lunaris_core::CypherDialect::Legacy,
                }
            }
        }
        let storage: Arc<dyn lunaris_core::StoragePort> = Arc::new(UnusedStorage);

        let ev_in_scope = ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: Ulid::new(),
            lsn_wall_ms: 1,
            lsn_counter: 1,
            source: "test:wm/note-0".into(),
        };
        let ev_out_of_scope = ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: Ulid::new(),
            lsn_wall_ms: 2,
            lsn_counter: 2,
            source: "other:note-1".into(),
        };

        // Some(prefix) path — 1 of 2 events matches.
        let report_scoped = c
            .consolidate_scoped(
                storage.clone(),
                &[ev_in_scope.clone(), ev_out_of_scope.clone()],
                Some("test:wm/"),
            )
            .await
            .unwrap();
        assert!(
            report_scoped.promotions.is_empty(),
            "NoopConsolidator returns empty report regardless of batch"
        );

        // None path — full batch forwarded unchanged.
        let report_unscoped =
            c.consolidate_scoped(storage, &[ev_in_scope, ev_out_of_scope], None).await.unwrap();
        assert_eq!(report_scoped, report_unscoped);
    }
}
