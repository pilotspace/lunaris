//! Plan 04-04: Runtime toggle for the slow-path Verifier worker.
//!
//! Mirrors Plan 03-03 [`crate::graph_pipeline::GraphPipelineHandle`] verbatim
//! with verifier renames per the plan's `critical_constraints` — same
//! "three equivalent toggle surfaces" (code / env / future config), same
//! `parking_lot::RwLock<bool>` lock discipline, same idempotent D-12
//! observability semantics.
//!
//! Unlike the graph pipeline handle, this one OWNS a tokio worker lifecycle:
//! `enable()` spawns one [`lunaris_verify::run_verify_worker`] task and
//! `disable()` signals shutdown via [`tokio::sync::Notify`] then
//! [`Self::join_worker`] joins.
//!
//! ## Surfaces (D-08/D-10)
//!
//! 1. Code: `handle.verify_pipeline().enable()` / `.disable()`
//! 2. Env: `LUNARIS_VERIFY_ENABLED=1` at `Lunaris::open(url)` time
//! 3. (Future) Config: `lunaris.config.verify.enabled = true` — Phase 5 OPS-08
//!    structured config lands the TOML reader.
//!
//! ## B-10 late-bound storage
//!
//! The handle is constructed BEFORE `StoragePort` in the `Lunaris::open` flow.
//! To spawn the worker we need storage, so the handle exposes
//! [`Self::bind_storage`] that the outer constructor calls after the
//! `Arc<dyn StoragePort>` exists. `enable()` only spawns the worker when
//! storage is bound — otherwise logs a warning and proceeds (the toggle flips
//! but the worker stays unsaddled; caller visibility via `worker_handle.is_none()`).
//!
//! ## D-26 zero-overhead-when-OFF
//!
//! With the pipeline OFF (default), zero worker tasks are spawned, no
//! subscribe call fires, no dyn-verifier allocation is made beyond the
//! [`NoopVerifier`] stored in the handle. Turn-on cost is a single
//! `tokio::spawn` + one `subscribe` call.

use std::sync::Arc;

use lunaris_core::HlcClock;
use lunaris_verify::{NoopVerifier, Verifier};
use parking_lot::{Mutex, RwLock};

/// Process-env knob (D-08/D-10) — `LUNARIS_VERIFY_ENABLED=1|true|on` flips the
/// initial state of [`VerifierPipelineHandle`] at [`crate::Lunaris::open`] time.
/// Anything else (including unset) keeps the pipeline OFF (blueprint §5.1
/// default).
pub const ENABLED_ENV_VAR: &str = "LUNARIS_VERIFY_ENABLED";

/// The single switch (D-08). Default state is OFF unless
/// `LUNARIS_VERIFY_ENABLED=1` is set in env at construction time.
///
/// The handle owns (a) the toggle bit, (b) the [`Arc<dyn Verifier>`] used by
/// the worker, (c) the late-bound `Arc<dyn StoragePort>` needed for
/// `subscribe`, (d) the worker's [`tokio::task::JoinHandle`], and (e) the
/// shutdown [`tokio::sync::Notify`] channel.
pub struct VerifierPipelineHandle {
    /// Toggle bit. Read on every enable/disable call.
    enabled: RwLock<bool>,
    /// Verifier handle. Snapshotted via [`Self::snapshot_verifier`] which
    /// clones the `Arc` and drops the guard BEFORE any await.
    verifier: RwLock<Option<Arc<dyn Verifier>>>,
    /// State-change counter — atomically incremented on every `enable()` AND
    /// `disable()` that flips the bool. Phase 5 OPS-06 will expose this via
    /// `prometheus::IntCounter` without changing the public API.
    state_change_count: std::sync::atomic::AtomicU64,
    /// Plan 04 D-04 worker shutdown signal. `disable()` calls
    /// `shutdown.notify_one()`.
    shutdown: Arc<tokio::sync::Notify>,
    /// JoinHandle of the spawned worker. `None` until first `enable()` OR
    /// after `disable()` + `join_worker()`.
    worker_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Storage handle the worker subscribes through. Late-bound by
    /// [`crate::Lunaris::open`] via [`Self::bind_storage`].
    storage: RwLock<Option<Arc<dyn lunaris_core::StoragePort>>>,
    /// Plan 04-04 Task 4 (B-2): HlcClock for the worker's apply_supersede
    /// to call `clock.tick()` when stamping winner/loser bt. Late-bound by
    /// [`crate::Lunaris::open`] via [`Self::bind_clock`] alongside storage.
    clock: RwLock<Option<Arc<HlcClock>>>,
}

impl VerifierPipelineHandle {
    /// Construct a fresh handle. `initial_enabled` is read from
    /// `LUNARIS_VERIFY_ENABLED=1|0` by [`crate::Lunaris::open`] (D-08);
    /// `verifier` is typically [`NoopVerifier`] for the v0 default-OFF
    /// contract — callers wire a real backend via
    /// [`crate::Lunaris::with_verifier`].
    pub fn new(initial_enabled: bool, verifier: Arc<dyn Verifier>) -> Self {
        Self {
            enabled: RwLock::new(initial_enabled),
            verifier: RwLock::new(Some(verifier)),
            state_change_count: std::sync::atomic::AtomicU64::new(0),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            worker_handle: Mutex::new(None),
            storage: RwLock::new(None),
            clock: RwLock::new(None),
        }
    }

    /// Pure decision function — given a value (`None` = unset env var), return
    /// the toggle state. `"1"` / `"true"` / `"TRUE"` / `"on"` / `"ON"` → on;
    /// anything else → off.
    ///
    /// Pure fn so tests pass `Some("1")` / `None` explicitly without mutating
    /// process env. Edition 2024 + `#![forbid(unsafe_code)]` makes
    /// [`std::env::set_var`] / `remove_var` `unsafe`; the pure fn dodges this
    /// AND the parallel-test race.
    pub fn initial_state_from_value(raw: Option<&str>) -> bool {
        matches!(raw, Some("1" | "true" | "TRUE" | "on" | "ON"))
    }

    /// Convenience wrapper that reads [`ENABLED_ENV_VAR`] from process env
    /// and delegates to [`Self::initial_state_from_value`]. ONLY called by
    /// [`crate::Lunaris::open`] at construction time — NEVER from tests.
    pub fn initial_state_from_env() -> bool {
        Self::initial_state_from_value(std::env::var(ENABLED_ENV_VAR).ok().as_deref())
    }

    /// B-10 fix — late-bind the storage handle. Called by
    /// [`crate::Lunaris::open`] + [`crate::Lunaris::with_parts`] /
    /// [`crate::Lunaris::with_parts_keyword`] after constructing the
    /// `Arc<dyn StoragePort>`.
    pub fn bind_storage(&self, storage: Arc<dyn lunaris_core::StoragePort>) {
        *self.storage.write() = Some(storage);
    }

    /// Plan 04-04 Task 4 (B-2): late-bind the HlcClock the worker uses
    /// for `apply_supersede`'s `clock.tick()` call. The handle is
    /// constructed BEFORE the umbrella `Lunaris::open` knows the clock
    /// (the clock is created in the same construction sequence), so we
    /// bind it after construction the same way as `bind_storage`.
    pub fn bind_clock(&self, clock: Arc<HlcClock>) {
        *self.clock.write() = Some(clock);
    }

    /// Spawn the worker if one isn't already running AND storage is bound.
    /// Otherwise logs a warning + no-ops. Called by [`Self::enable`] on a
    /// real state transition.
    ///
    /// T-04-04-04 + T-04-04-07 mitigation: takes the worker_handle mutex
    /// BEFORE checking OR spawning, so a racing `enable()` + `enable()` can't
    /// produce two workers.
    pub(crate) fn spawn_worker_if_idle(&self) {
        let mut wh = self.worker_handle.lock();
        if wh.is_some() {
            return;
        }
        let storage = match self.storage.read().clone() {
            Some(s) => s,
            None => {
                tracing::warn!("verify_pipeline_enable_without_storage; worker not spawned");
                return;
            }
        };
        // Plan 04-04 Task 4 (B-2): clock is required by run_verify_worker
        // for apply_supersede's tick(). If unbound (test seam that didn't
        // call bind_clock), construct a fresh node-0 HlcClock so the worker
        // still spawns + the ON-OFF observability shape holds.
        let clock = self.clock.read().clone().unwrap_or_else(|| HlcClock::new(0));
        let verifier = self
            .snapshot_verifier()
            .unwrap_or_else(|| Arc::new(NoopVerifier) as Arc<dyn Verifier>);
        let shutdown = self.shutdown.clone();
        let handle = tokio::spawn(async move {
            match lunaris_verify::run_verify_worker(storage, verifier, shutdown, clock).await {
                Ok(jh) => {
                    // The inner tokio::spawn's JoinHandle drives the event
                    // loop; await it so our outer handle.await signals
                    // full-drain completion.
                    if let Err(e) = jh.await {
                        tracing::warn!(err = %e, "verify_pipeline_inner_worker_join_failed");
                    }
                }
                Err(e) => {
                    tracing::error!(err = %e, "verify_pipeline_worker_spawn_failed");
                }
            }
        });
        *wh = Some(handle);
    }

    /// Turn verifier worker ON. Idempotent — re-enabling a running pipeline
    /// is a no-op. Emits `tracing::info!(state = "enabled",
    /// "verify_pipeline_state_changed")` only on a real state transition.
    pub fn enable(&self) {
        let mut w = self.enabled.write();
        if !*w {
            *w = true;
            self.state_change_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tracing::info!(state = "enabled", "verify_pipeline_state_changed");
            drop(w);
            self.spawn_worker_if_idle();
        }
    }

    /// Turn verifier worker OFF. Idempotent — see [`Self::enable`] for
    /// observability semantics. Signals shutdown via
    /// [`tokio::sync::Notify::notify_one`] so the worker drain-loop + exit.
    /// Callers that need to guarantee the worker task has EXITED call
    /// [`Self::join_worker`] after `disable()`.
    pub fn disable(&self) {
        let mut w = self.enabled.write();
        if *w {
            *w = false;
            self.state_change_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tracing::info!(state = "disabled", "verify_pipeline_state_changed");
            drop(w);
            self.shutdown.notify_one();
        }
    }

    /// Await the spawned worker task to full exit. After `disable()` returns,
    /// the worker has been SIGNALED to shut down but may still be mid-drain.
    /// Callers that want a hard stop call `handle.verify_pipeline().disable()`
    /// then `.join_worker().await`.
    pub async fn join_worker(&self) {
        let handle = self.worker_handle.lock().take();
        if let Some(h) = handle {
            if let Err(e) = h.await {
                tracing::warn!(err = %e, "verify_pipeline_worker_join_failed");
            }
        }
    }

    /// Current toggle state.
    pub fn is_enabled(&self) -> bool {
        *self.enabled.read()
    }

    /// Total number of real state transitions since handle construction.
    pub fn state_change_count(&self) -> u64 {
        self.state_change_count.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Replace the verifier on this handle. Toggle state + state-change
    /// counter preserved (D-12 idempotent observability).
    pub fn set_verifier(&self, verifier: Arc<dyn Verifier>) {
        *self.verifier.write() = Some(verifier);
        tracing::info!("verify_pipeline_verifier_replaced");
    }

    /// CLAUDE.md "never hold a lock across `.await`": clone the `Arc` out of
    /// the read guard and return it. The guard is dropped at function return.
    pub fn snapshot_verifier(&self) -> Option<Arc<dyn Verifier>> {
        self.verifier.read().clone()
    }

    /// Convenience installer used by test seams that don't want to wire a
    /// real verifier.
    pub fn with_noop() -> Self {
        Self::new(false, Arc::new(NoopVerifier) as Arc<dyn Verifier>)
    }
}

impl std::fmt::Debug for VerifierPipelineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifierPipelineHandle")
            .field("enabled", &*self.enabled.read())
            .field("has_verifier", &self.verifier.read().is_some())
            .field("has_storage", &self.storage.read().is_some())
            .field("has_clock", &self.clock.read().is_some())
            .field("has_worker", &self.worker_handle.lock().is_some())
            .field("state_change_count", &self.state_change_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // All tests use the pure `initial_state_from_value` helper with explicit
    // `Option` values — Edition 2024 + #![forbid(unsafe_code)] disallows
    // mutating process env, and parallel test execution would race anyway.

    #[test]
    fn default_state_is_off_when_value_none() {
        assert!(!VerifierPipelineHandle::initial_state_from_value(None));
    }

    #[test]
    fn value_one_enables_initial_state() {
        assert!(VerifierPipelineHandle::initial_state_from_value(Some("1")));
        assert!(VerifierPipelineHandle::initial_state_from_value(Some("true")));
        assert!(VerifierPipelineHandle::initial_state_from_value(Some("TRUE")));
        assert!(VerifierPipelineHandle::initial_state_from_value(Some("on")));
        assert!(VerifierPipelineHandle::initial_state_from_value(Some("ON")));
    }

    #[test]
    fn value_off_disables_initial_state() {
        assert!(!VerifierPipelineHandle::initial_state_from_value(Some("0")));
        assert!(!VerifierPipelineHandle::initial_state_from_value(Some("false")));
        assert!(!VerifierPipelineHandle::initial_state_from_value(Some("")));
        assert!(!VerifierPipelineHandle::initial_state_from_value(Some("yes")));
        assert!(!VerifierPipelineHandle::initial_state_from_value(Some("True")));
    }

    #[tokio::test]
    async fn enable_disable_is_observable_and_idempotent() {
        let h = VerifierPipelineHandle::with_noop();
        assert!(!h.is_enabled());
        assert_eq!(h.state_change_count(), 0);

        h.enable();
        assert!(h.is_enabled());
        assert_eq!(h.state_change_count(), 1);

        // Double-enable: idempotent — no double increment.
        h.enable();
        assert_eq!(h.state_change_count(), 1);

        h.disable();
        assert!(!h.is_enabled());
        assert_eq!(h.state_change_count(), 2);

        // Double-disable: idempotent.
        h.disable();
        assert_eq!(h.state_change_count(), 2);

        // ON → OFF → ON sequence — three real transitions total.
        h.enable();
        assert!(h.is_enabled());
        assert_eq!(h.state_change_count(), 3);
    }

    #[test]
    fn snapshot_verifier_returns_arc_clone() {
        let h = VerifierPipelineHandle::new(true, Arc::new(NoopVerifier));
        let snap1 = h.snapshot_verifier();
        let snap2 = h.snapshot_verifier();
        assert!(snap1.is_some());
        assert!(snap2.is_some());
        assert!(Arc::ptr_eq(snap1.as_ref().unwrap(), snap2.as_ref().unwrap()));
    }

    #[test]
    fn set_verifier_replaces_handle_preserving_toggle() {
        let h = VerifierPipelineHandle::with_noop();
        h.enable();
        assert_eq!(h.state_change_count(), 1);
        assert!(h.is_enabled());

        let replacement: Arc<dyn Verifier> = Arc::new(NoopVerifier);
        h.set_verifier(replacement);

        assert!(h.is_enabled(), "set_verifier must not flip the toggle");
        assert_eq!(
            h.state_change_count(),
            1,
            "set_verifier must not increment state changes"
        );
    }

    #[test]
    fn debug_impl_is_safe_to_format() {
        let h = VerifierPipelineHandle::with_noop();
        let dbg = format!("{:?}", h);
        assert!(dbg.contains("enabled"));
        assert!(dbg.contains("has_verifier"));
        assert!(dbg.contains("has_storage"));
        assert!(dbg.contains("state_change_count"));
    }

    /// B-10: enabling a handle without storage bound does NOT spawn a worker,
    /// and does NOT panic. Storage-less enable is a soft failure with
    /// tracing::warn! — the toggle flips but the worker stays unsaddled.
    #[tokio::test]
    async fn enable_without_bound_storage_does_not_spawn_worker() {
        let h = VerifierPipelineHandle::new(false, Arc::new(NoopVerifier));
        h.enable();
        assert!(h.is_enabled());
        assert!(
            h.worker_handle.lock().is_none(),
            "no storage bound → no worker spawned (B-10)"
        );
    }

    /// B-10: the `new()` body initializes all 6 fields inline. The struct is
    /// constructible via the public constructor and every field is visible
    /// through its public accessor.
    #[test]
    fn new_initializes_all_six_fields() {
        let h = VerifierPipelineHandle::new(false, Arc::new(NoopVerifier));
        assert!(!h.is_enabled(), "enabled bit");
        assert!(h.snapshot_verifier().is_some(), "verifier slot");
        assert_eq!(h.state_change_count(), 0, "state_change_count");
        // shutdown / worker_handle / storage are private but reachable via
        // their side-effects: bind_storage + spawn_worker_if_idle + notify.
        assert!(h.storage.read().is_none(), "storage unbound by default");
        assert!(h.worker_handle.lock().is_none(), "worker_handle None by default");
    }
}
