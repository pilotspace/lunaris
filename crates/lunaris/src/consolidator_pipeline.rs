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

use lunaris_consolidate::{ActRConsolidator, Consolidator, NoopConsolidator};
use lunaris_core::{LunarisError, StorageError};
use parking_lot::{Mutex, RwLock};

/// Process-env knob (D-08/D-10) — `LUNARIS_CONSOLIDATE_ENABLED=1|true|on`
/// flips the initial state at [`crate::Lunaris::open`] time.
pub const ENABLED_ENV_VAR: &str = "LUNARIS_CONSOLIDATE_ENABLED";

/// Phase 16-01 (CONSOL-V1-01) — backend-selection env var.
///
/// Resolved values:
/// - unset or `"actr"` (case-insensitive, trimmed) → [`ActRConsolidator`]
///   (production default per CONSOL-V1-01 D-01).
/// - `"noop"` (case-insensitive, trimmed) → [`NoopConsolidator`]
///   (operator opt-out preserved per D-02 three-surface toggle).
/// - anything else → fail-fast with [`LunarisError::Config`] (NO silent
///   fallback — unknown values are a configuration error, not a hint).
///
/// The corresponding resolver is [`ConsolidatorPipelineHandle::backend_from_env`].
pub const BACKEND_ENV_VAR: &str = "LUNARIS_CONSOLIDATOR_BACKEND";

/// One-shot guard so the `consolidator_backend_resolved` info log fires exactly
/// once per process (avoids log-spam when `with_parts`/`with_parts_keyword`
/// test seams resolve the backend for every constructed Lunaris handle).
/// R16-05 mitigation — Helios observability needs exactly one line.
static BACKEND_LOG_ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// The single switch (D-08). Default state is OFF unless
/// `LUNARIS_CONSOLIDATE_ENABLED=1` is set in env at construction time.
///
/// Phase 12 HELIOS-04 additive: the handle now also carries a
/// `scope_prefix: RwLock<Option<String>>` that gates promotion at the
/// worker level by `event.source.starts_with(prefix)`. `None` (default)
/// preserves the v0.1.0 system-wide semantics — every existing caller
/// that uses [`Self::enable`] sees no behavioural change.
pub struct ConsolidatorPipelineHandle {
    enabled: RwLock<bool>,
    consolidator: RwLock<Option<Arc<dyn Consolidator>>>,
    state_change_count: std::sync::atomic::AtomicU64,
    shutdown: Arc<tokio::sync::Notify>,
    worker_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    storage: RwLock<Option<Arc<dyn lunaris_core::StoragePort>>>,
    /// Phase 12 HELIOS-04 — hard prefix match on `ConsolidateEvent.source`
    /// applied inside the worker via [`lunaris_consolidate::Consolidator::consolidate_scoped`].
    /// `None` = system-wide (v0.1.0 semantics); `Some(prefix)` = only events
    /// whose `source.starts_with(prefix)` are promoted.
    scope_prefix: RwLock<Option<String>>,
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
            scope_prefix: RwLock::new(None),
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
                tracing::warn!("consolidator_pipeline_enable_without_storage; worker not spawned");
                return;
            }
        };
        let consolidator = self
            .snapshot_consolidator()
            .unwrap_or_else(|| Arc::new(NoopConsolidator) as Arc<dyn Consolidator>);
        let shutdown = self.shutdown.clone();
        // Phase 12 HELIOS-04 — snapshot the scope prefix at spawn time and
        // plumb it into the worker. `None` preserves v0.1.0 system-wide
        // semantics; `Some(prefix)` activates the hard prefix filter.
        let source_prefix = self.scope_prefix.read().clone();
        let handle = tokio::spawn(async move {
            // RFC 0001 Wave 3F: ConsolidateSupervisor is the new per-scope
            // entrypoint; the pipeline wrapper still uses the single-topic
            // legacy worker for backwards compat. Migrating this pipeline to
            // use the supervisor is a v0.3 task (requires per-scope scope_prefix
            // routing through ConsolidatorPipelineHandle).
            #[allow(deprecated)]
            match lunaris_consolidate::run_consolidate_worker(
                storage,
                consolidator,
                shutdown,
                source_prefix,
            )
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
    ///
    /// Phase 12 HELIOS-04 — `enable()` is the **system-wide** path; calling
    /// it clears any previously-set `scope_prefix` so operators toggling
    /// enable→disable→enable never re-activate stale scope accidentally.
    /// Use [`Self::enable_for_scope`] when Helios-only scope is required.
    pub fn enable(&self) {
        *self.scope_prefix.write() = None;
        let mut w = self.enabled.write();
        if !*w {
            *w = true;
            self.state_change_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tracing::info!(
                state = "enabled",
                scope = tracing::field::Empty,
                "consolidator_pipeline_state_changed"
            );
            drop(w);
            self.spawn_worker_if_idle();
        }
    }

    /// Phase 12 HELIOS-04 — turn the consolidator worker ON with a hard
    /// prefix filter on `ConsolidateEvent.source`. Only events where
    /// `event.source.starts_with(prefix)` are forwarded to
    /// [`lunaris_consolidate::Consolidator::consolidate`] at flush time;
    /// everything else is dropped BEFORE any `AuditEvent::ConsolidatorPromotion`
    /// could be emitted (T-12-02-02 mitigation — cross-tenant audit leak).
    ///
    /// Semantics:
    ///
    /// * Idempotent when called with the same `prefix` the handle already
    ///   holds — no state-change bump, no worker respawn.
    /// * Prefix rotation (new `prefix` != stored `prefix`) forces a
    ///   disable → enable cycle so the newly-spawned worker picks up the
    ///   new filter. `state_change_count` bumps twice (one disable, one
    ///   enable).
    /// * Empty `prefix` degrades to [`Self::enable`] (system-wide) with a
    ///   `tracing::warn` so the hygiene signal is visible in logs
    ///   (T-12-02-05 mitigation).
    /// * Match rule is `str::starts_with` — NO regex, NO glob. Establishes
    ///   D-04: `enable_for_scope("helios:fs/")` means "only promote rows
    ///   where `source LIKE 'helios:fs/%'`".
    pub fn enable_for_scope(&self, prefix: impl Into<String>) {
        let prefix: String = prefix.into();
        if prefix.is_empty() {
            tracing::warn!(
                "consolidator_pipeline_enable_for_scope_empty_prefix; \
                 degrading to system-wide enable (T-12-02-05 hygiene signal)"
            );
            self.enable();
            return;
        }

        // Check if the handle is already scope-ON with this exact prefix.
        // Idempotent no-op to match `enable()`'s contract.
        let current = self.scope_prefix.read().clone();
        let already_scoped_same = matches!(&current, Some(p) if p == &prefix);
        let is_enabled = *self.enabled.read();
        if already_scoped_same && is_enabled {
            return;
        }

        // Prefix rotation: if a DIFFERENT scope was active, cycle the worker
        // so the new filter takes effect on the fresh spawn. Empty prior
        // scope = fresh enable path, no rotation needed.
        let needs_rotation = is_enabled && matches!(&current, Some(p) if p != &prefix);
        if needs_rotation {
            self.disable();
            // `disable()` clears scope_prefix as a side-effect; we'll re-set
            // it below before re-enabling so the spawn picks up the new one.
        }

        // Install the new scope prefix BEFORE flipping enabled so
        // `spawn_worker_if_idle` reads the correct value.
        *self.scope_prefix.write() = Some(prefix.clone());

        let mut w = self.enabled.write();
        if !*w {
            *w = true;
            self.state_change_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tracing::info!(
                state = "enabled",
                scope = %prefix,
                "consolidator_pipeline_state_changed"
            );
            drop(w);
            self.spawn_worker_if_idle();
        }
    }

    /// Turn consolidator worker OFF. Idempotent. Signals shutdown via
    /// [`tokio::sync::Notify::notify_one`].
    ///
    /// Phase 12 HELIOS-04 — clears any stored `scope_prefix`. Every
    /// re-enable is explicit: system-wide via [`Self::enable`], or scoped
    /// via [`Self::enable_for_scope`].
    pub fn disable(&self) {
        let mut w = self.enabled.write();
        if *w {
            *w = false;
            self.state_change_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tracing::info!(state = "disabled", "consolidator_pipeline_state_changed");
            drop(w);
            self.shutdown.notify_one();
        }
        // Always clear scope — disable is the reset point. Done AFTER the
        // enabled-write lock drops so the two fields never rev in lockstep
        // while held together.
        *self.scope_prefix.write() = None;
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

    /// Phase 12 HELIOS-04 — snapshot reader for the current scope prefix.
    /// `None` means system-wide (v0.1.0 semantics); `Some(prefix)` means the
    /// worker is filtering promotion by `event.source.starts_with(prefix)`.
    pub fn scope_prefix(&self) -> Option<String> {
        self.scope_prefix.read().clone()
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

    /// Phase 16-01 (CONSOL-V1-01) — convenience installer wiring the real
    /// [`ActRConsolidator`] with [`ActRConsolidator::default`] parameters
    /// (Anderson 1996: `d=0.5`, archive `-0.5`, promote `+1.0`, noise `0.0`).
    /// Mirrors [`Self::with_noop`]; used by [`Self::backend_from_env`].
    pub fn with_actr() -> Self {
        Self::new(false, Arc::new(ActRConsolidator::default()) as Arc<dyn Consolidator>)
    }

    /// Phase 16-01 (CONSOL-V1-01) — resolve the default backend from the
    /// [`BACKEND_ENV_VAR`] env var. See the `BACKEND_ENV_VAR` doc for the
    /// resolution table. Fails fast on unknown values (per 16-01 instruction:
    /// silent fallback would violate the three-surface toggle's trust
    /// contract — operators MUST know their env typo was rejected).
    ///
    /// Emits exactly one `tracing::info!(consolidator_backend = ...)` per
    /// process (R16-05 mitigation — Helios operators see the flip in logs).
    ///
    /// Returns an `Arc<dyn Consolidator>` ready to hand to [`Self::new`] (or
    /// to `set_consolidator` on an already-constructed handle).
    pub fn backend_from_env() -> Result<Arc<dyn Consolidator>, LunarisError> {
        let raw = std::env::var(BACKEND_ENV_VAR).ok();
        let trimmed = raw.as_deref().map(|v| v.trim());
        let (backend, name): (Arc<dyn Consolidator>, &'static str) = match trimmed {
            None | Some("") => {
                (Arc::new(ActRConsolidator::default()) as Arc<dyn Consolidator>, "ActRConsolidator")
            }
            Some(v) if v.eq_ignore_ascii_case("actr") => {
                (Arc::new(ActRConsolidator::default()) as Arc<dyn Consolidator>, "ActRConsolidator")
            }
            Some(v) if v.eq_ignore_ascii_case("noop") => {
                (Arc::new(NoopConsolidator) as Arc<dyn Consolidator>, "NoopConsolidator")
            }
            Some(other) => {
                // No `LunarisError::Config` variant exists; route through
                // `StorageError::Backend` (matches the crate's existing
                // string-carrying misconfig path) so the full error still
                // carries our env-var context and [`CONSOL-V1-01`] tag.
                return Err(LunarisError::Storage(StorageError::Backend(format!(
                    "{BACKEND_ENV_VAR}={other:?} is not one of [actr, noop]; \
                     unset for the ActR default, or set to \"noop\" to pin the \
                     no-op backend. CONSOL-V1-01."
                ))));
            }
        };
        // R16-05 — one-shot info log so the flip is visible to downstream
        // Helios observability without spamming per-handle construction.
        BACKEND_LOG_ONCE.get_or_init(|| {
            tracing::info!(
                target: "lunaris::consolidator",
                consolidator_backend = name,
                "consolidator_backend_resolved"
            );
        });
        Ok(backend)
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
        assert_eq!(h.state_change_count(), 1, "set_consolidator must not increment state changes");
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
        assert!(h.worker_handle.lock().is_none(), "no storage bound → no worker spawned (B-10)");
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
        assert!(h.scope_prefix().is_none(), "scope_prefix None by default (v0.1.0 parity)");
    }

    // -----------------------------------------------------------------------
    // Phase 12 HELIOS-04 — scope-filter unit tests.
    // -----------------------------------------------------------------------

    /// enable_for_scope stores the prefix AND flips enabled=true on first call.
    #[tokio::test]
    async fn enable_for_scope_sets_prefix() {
        let h = ConsolidatorPipelineHandle::with_noop();
        assert!(!h.is_enabled());
        assert!(h.scope_prefix().is_none());

        h.enable_for_scope("helios:fs/");
        assert!(h.is_enabled(), "enable_for_scope turns the pipeline ON");
        assert_eq!(
            h.scope_prefix(),
            Some("helios:fs/".to_string()),
            "scope_prefix stored verbatim"
        );
        assert_eq!(h.state_change_count(), 1, "one state transition");
    }

    /// enable() (system-wide) resets any previously-stored scope_prefix so a
    /// stale Helios scope can't leak into a system-wide re-enable (D-04).
    #[tokio::test]
    async fn enable_clears_scope_prefix() {
        let h = ConsolidatorPipelineHandle::with_noop();
        h.enable_for_scope("helios:fs/");
        assert_eq!(h.scope_prefix(), Some("helios:fs/".to_string()));

        // Toggle off then back on via system-wide enable.
        h.disable();
        h.enable();
        assert!(h.scope_prefix().is_none(), "enable() (system-wide) clears any stale scope_prefix");
    }

    /// Re-calling enable_for_scope with the SAME prefix on an already-scoped
    /// handle is a no-op — state_change_count stays flat, no worker churn.
    #[tokio::test]
    async fn enable_for_scope_idempotent_same_prefix() {
        let h = ConsolidatorPipelineHandle::with_noop();
        h.enable_for_scope("helios:fs/");
        assert_eq!(h.state_change_count(), 1);

        h.enable_for_scope("helios:fs/");
        assert_eq!(
            h.state_change_count(),
            1,
            "idempotent: same prefix must not bump state_change_count"
        );
        assert_eq!(h.scope_prefix(), Some("helios:fs/".to_string()));
    }

    /// Rotating to a DIFFERENT prefix cycles the worker: counter bumps by 2
    /// (one disable, one enable) so operators can observe the rotation in
    /// `state_change_count` telemetry.
    #[tokio::test]
    async fn enable_for_scope_rotate_prefix_reconfigures_worker() {
        let h = ConsolidatorPipelineHandle::with_noop();
        h.enable_for_scope("helios:fs/");
        assert_eq!(h.state_change_count(), 1);
        assert_eq!(h.scope_prefix(), Some("helios:fs/".to_string()));

        h.enable_for_scope("other:tenant/");
        // Rotation = disable (+1) + enable (+1) on top of the initial enable (+1).
        assert_eq!(
            h.state_change_count(),
            3,
            "rotation cycles the worker: disable + re-enable bumps counter by 2"
        );
        assert_eq!(h.scope_prefix(), Some("other:tenant/".to_string()));
        assert!(h.is_enabled());
    }

    /// T-12-02-05 — `enable_for_scope("")` degrades to system-wide `enable()`
    /// AND logs a warn. Scope ends up `None` (not `Some("")`) so filter
    /// comparison downstream reads cleanly.
    #[tokio::test]
    async fn enable_for_scope_empty_prefix_degrades_to_system_wide() {
        let h = ConsolidatorPipelineHandle::with_noop();
        h.enable_for_scope("");
        assert!(h.is_enabled());
        assert!(
            h.scope_prefix().is_none(),
            "empty prefix MUST degrade to system-wide (scope_prefix=None), \
             not leak a `Some(\"\")` value that would match everything"
        );
    }

    /// disable() is the reset point — clears scope_prefix so every re-enable
    /// (system-wide OR scoped) is explicit.
    #[tokio::test]
    async fn disable_clears_scope_prefix() {
        let h = ConsolidatorPipelineHandle::with_noop();
        h.enable_for_scope("helios:fs/");
        assert_eq!(h.scope_prefix(), Some("helios:fs/".to_string()));
        h.disable();
        assert!(h.scope_prefix().is_none(), "disable MUST clear scope_prefix");
    }
}
