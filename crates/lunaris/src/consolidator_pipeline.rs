//! Plan 04-04: Runtime toggle for the ACT-R Consolidator worker.
//!
//! Mirrors [`crate::verify_pipeline::VerifierPipelineHandle`] verbatim with
//! consolidator renames per the plan's `critical_constraints` — same
//! "three equivalent toggle surfaces" (code / env / future config), same
//! `parking_lot::RwLock<bool>` lock discipline, same idempotent D-12
//! observability semantics.
//!
//! `enable()` spawns one [`lunaris_consolidate::run_consolidate_worker`]
//! task; `disable()` signals shutdown via [`tokio::sync::Notify`] and
//! [`Self::join_worker`] joins.
//!
//! ## Surfaces (D-08/D-10)
//!
//! 1. Code: `handle.consolidator_pipeline().enable()` / `.disable()`
//! 2. Env: `LUNARIS_CONSOLIDATE_ENABLED=1` at `Lunaris::open(url)` time
//! 3. (Future) Config: `lunaris.config.consolidate.enabled = true` —
//!    Phase 5 OPS-08.
//!
//! ## B-10 late-bound storage
//!
//! Same late-bind pattern as [`crate::verify_pipeline::VerifierPipelineHandle`]
//! — the handle is constructed BEFORE storage, and the outer constructor
//! calls [`Self::bind_storage`] after the `Arc<dyn StoragePort>` exists.
//!
//! ## D-26 zero-overhead-when-OFF
//!
//! With the pipeline OFF (default), zero worker tasks are spawned, no
//! subscribe fires, no dyn-consolidator allocation beyond the
//! [`NoopConsolidator`] stored in the handle.

use std::sync::Arc;

use lunaris_consolidate::{Consolidator, NoopConsolidator};
use parking_lot::{Mutex, RwLock};

/// Process-env knob (D-08/D-10) — `LUNARIS_CONSOLIDATE_ENABLED=1|true|on`
/// flips the initial state at [`crate::Lunaris::open`] time.
pub const ENABLED_ENV_VAR: &str = "LUNARIS_CONSOLIDATE_ENABLED";

/// The single switch (D-08). Default state is OFF unless
/// `LUNARIS_CONSOLIDATE_ENABLED=1` is set in env at construction time.
pub struct ConsolidatorPipelineHandle {
    enabled: RwLock<bool>,
    consolidator: RwLock<Option<Arc<dyn Consolidator>>>,
    state_change_count: std::sync::atomic::AtomicU64,
    shutdown: Arc<tokio::sync::Notify>,
    worker_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    storage: RwLock<Option<Arc<dyn lunaris_core::StoragePort>>>,
}

impl ConsolidatorPipelineHandle {
    /// Construct a fresh handle. `initial_enabled` is read from
    /// `LUNARIS_CONSOLIDATE_ENABLED=1|0` by [`crate::Lunaris::open`] (D-08);
    /// `consolidator` is typically [`NoopConsolidator`] for the v0 default-OFF
    /// contract — callers wire a real backend via
    /// [`crate::Lunaris::with_consolidator`].
    pub fn new(initial_enabled: bool, consolidator: Arc<dyn Consolidator>) -> Self {
        Self {
            enabled: RwLock::new(initial_enabled),
            consolidator: RwLock::new(Some(consolidator)),
            state_change_count: std::sync::atomic::AtomicU64::new(0),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            worker_handle: Mutex::new(None),
            storage: RwLock::new(None),
        }
    }

    /// Pure decision function — same shape as the verify-pipeline version.
    pub fn initial_state_from_value(raw: Option<&str>) -> bool {
        matches!(raw, Some("1" | "true" | "TRUE" | "on" | "ON"))
    }

    /// Convenience wrapper — ONLY called by [`crate::Lunaris::open`] at
    /// construction time.
    pub fn initial_state_from_env() -> bool {
        Self::initial_state_from_value(std::env::var(ENABLED_ENV_VAR).ok().as_deref())
    }

    /// B-10 fix — late-bind the storage handle.
    pub fn bind_storage(&self, storage: Arc<dyn lunaris_core::StoragePort>) {
        *self.storage.write() = Some(storage);
    }

    /// Spawn the worker if one isn't already running AND storage is bound.
    /// Same race-free contract as the verify-pipeline spawn path.
    pub(crate) fn spawn_worker_if_idle(&self) {
        let mut wh = self.worker_handle.lock();
        if wh.is_some() {
            return;
        }
        let storage = match self.storage.read().clone() {
            Some(s) => s,
            None => {
                tracing::warn!(
                    "consolidator_pipeline_enable_without_storage; worker not spawned"
                );
                return;
            }
        };
        let consolidator = self
            .snapshot_consolidator()
            .unwrap_or_else(|| Arc::new(NoopConsolidator) as Arc<dyn Consolidator>);
        let shutdown = self.shutdown.clone();
        let handle = tokio::spawn(async move {
            match lunaris_consolidate::run_consolidate_worker(storage, consolidator, shutdown)
                .await
            {
                Ok(jh) => {
                    if let Err(e) = jh.await {
                        tracing::warn!(err = %e, "consolidator_pipeline_inner_worker_join_failed");
                    }
                }
                Err(e) => {
                    tracing::error!(err = %e, "consolidator_pipeline_worker_spawn_failed");
                }
            }
        });
        *wh = Some(handle);
    }

    /// Turn consolidator worker ON. Idempotent.
    pub fn enable(&self) {
        let mut w = self.enabled.write();
        if !*w {
            *w = true;
            self.state_change_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tracing::info!(state = "enabled", "consolidator_pipeline_state_changed");
            drop(w);
            self.spawn_worker_if_idle();
        }
    }

    /// Turn consolidator worker OFF. Idempotent. Signals shutdown via
    /// [`tokio::sync::Notify::notify_one`].
    pub fn disable(&self) {
        let mut w = self.enabled.write();
        if *w {
            *w = false;
            self.state_change_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tracing::info!(state = "disabled", "consolidator_pipeline_state_changed");
            drop(w);
            self.shutdown.notify_one();
        }
    }

    /// Await the spawned worker task to full exit.
    pub async fn join_worker(&self) {
        let handle = self.worker_handle.lock().take();
        if let Some(h) = handle
            && let Err(e) = h.await
        {
            tracing::warn!(err = %e, "consolidator_pipeline_worker_join_failed");
        }
    }

    pub fn is_enabled(&self) -> bool {
        *self.enabled.read()
    }

    pub fn state_change_count(&self) -> u64 {
        self.state_change_count.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Replace the consolidator. Toggle state + state-change counter preserved.
    pub fn set_consolidator(&self, consolidator: Arc<dyn Consolidator>) {
        *self.consolidator.write() = Some(consolidator);
        tracing::info!("consolidator_pipeline_consolidator_replaced");
    }

    /// CLAUDE.md "never hold a lock across `.await`": clone the `Arc` out of
    /// the read guard and return it.
    pub fn snapshot_consolidator(&self) -> Option<Arc<dyn Consolidator>> {
        self.consolidator.read().clone()
    }

    /// Convenience installer used by test seams.
    pub fn with_noop() -> Self {
        Self::new(false, Arc::new(NoopConsolidator) as Arc<dyn Consolidator>)
    }
}

impl std::fmt::Debug for ConsolidatorPipelineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsolidatorPipelineHandle")
            .field("enabled", &*self.enabled.read())
            .field("has_consolidator", &self.consolidator.read().is_some())
            .field("has_storage", &self.storage.read().is_some())
            .field("has_worker", &self.worker_handle.lock().is_some())
            .field("state_change_count", &self.state_change_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_off_when_value_none() {
        assert!(!ConsolidatorPipelineHandle::initial_state_from_value(None));
    }

    #[test]
    fn value_one_enables_initial_state() {
        assert!(ConsolidatorPipelineHandle::initial_state_from_value(Some("1")));
        assert!(ConsolidatorPipelineHandle::initial_state_from_value(Some("true")));
        assert!(ConsolidatorPipelineHandle::initial_state_from_value(Some("TRUE")));
        assert!(ConsolidatorPipelineHandle::initial_state_from_value(Some("on")));
        assert!(ConsolidatorPipelineHandle::initial_state_from_value(Some("ON")));
    }

    #[test]
    fn value_off_disables_initial_state() {
        assert!(!ConsolidatorPipelineHandle::initial_state_from_value(Some("0")));
        assert!(!ConsolidatorPipelineHandle::initial_state_from_value(Some("false")));
        assert!(!ConsolidatorPipelineHandle::initial_state_from_value(Some("")));
        assert!(!ConsolidatorPipelineHandle::initial_state_from_value(Some("yes")));
        assert!(!ConsolidatorPipelineHandle::initial_state_from_value(Some("True")));
    }

    #[tokio::test]
    async fn enable_disable_is_observable_and_idempotent() {
        let h = ConsolidatorPipelineHandle::with_noop();
        assert!(!h.is_enabled());
        assert_eq!(h.state_change_count(), 0);

        h.enable();
        assert!(h.is_enabled());
        assert_eq!(h.state_change_count(), 1);

        h.enable();
        assert_eq!(h.state_change_count(), 1);

        h.disable();
        assert!(!h.is_enabled());
        assert_eq!(h.state_change_count(), 2);

        h.disable();
        assert_eq!(h.state_change_count(), 2);

        h.enable();
        assert!(h.is_enabled());
        assert_eq!(h.state_change_count(), 3);
    }

    #[test]
    fn snapshot_consolidator_returns_arc_clone() {
        let h = ConsolidatorPipelineHandle::new(true, Arc::new(NoopConsolidator));
        let snap1 = h.snapshot_consolidator();
        let snap2 = h.snapshot_consolidator();
        assert!(snap1.is_some());
        assert!(snap2.is_some());
        assert!(Arc::ptr_eq(snap1.as_ref().unwrap(), snap2.as_ref().unwrap()));
    }

    #[test]
    fn set_consolidator_replaces_handle_preserving_toggle() {
        let h = ConsolidatorPipelineHandle::with_noop();
        h.enable();
        assert_eq!(h.state_change_count(), 1);
        assert!(h.is_enabled());

        let replacement: Arc<dyn Consolidator> = Arc::new(NoopConsolidator);
        h.set_consolidator(replacement);

        assert!(h.is_enabled(), "set_consolidator must not flip the toggle");
        assert_eq!(
            h.state_change_count(),
            1,
            "set_consolidator must not increment state changes"
        );
    }

    #[test]
    fn debug_impl_is_safe_to_format() {
        let h = ConsolidatorPipelineHandle::with_noop();
        let dbg = format!("{:?}", h);
        assert!(dbg.contains("enabled"));
        assert!(dbg.contains("has_consolidator"));
        assert!(dbg.contains("has_storage"));
        assert!(dbg.contains("state_change_count"));
    }

    /// B-10: enabling a handle without storage bound does NOT spawn a worker.
    #[tokio::test]
    async fn enable_without_bound_storage_does_not_spawn_worker() {
        let h = ConsolidatorPipelineHandle::new(false, Arc::new(NoopConsolidator));
        h.enable();
        assert!(h.is_enabled());
        assert!(
            h.worker_handle.lock().is_none(),
            "no storage bound → no worker spawned (B-10)"
        );
    }

    /// B-10 explicit field init — all 6 fields visible through public accessors.
    #[test]
    fn new_initializes_all_six_fields() {
        let h = ConsolidatorPipelineHandle::new(false, Arc::new(NoopConsolidator));
        assert!(!h.is_enabled(), "enabled bit");
        assert!(h.snapshot_consolidator().is_some(), "consolidator slot");
        assert_eq!(h.state_change_count(), 0, "state_change_count");
        assert!(h.storage.read().is_none(), "storage unbound by default");
        assert!(h.worker_handle.lock().is_none(), "worker_handle None by default");
    }
}
