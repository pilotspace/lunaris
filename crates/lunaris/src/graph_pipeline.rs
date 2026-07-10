//! Plan 03-03: Runtime toggle for the graph extraction pipeline.
//!
//! Per blueprint §5.2 + ROADMAP Phase 3 success criterion #5, the graph
//! pipeline is OFF by default. With it OFF, [`crate::Lunaris::ingest`] stays
//! in the Phase 2 fast path; with it ON, ingest extracts entities, relations,
//! and facts then fans them into the SAME
//! [`lunaris_core::StoragePort::atomic_write`] call as the Episode and Chunks
//! (D-18 single-transaction contract).
//!
//! ## Surfaces (D-10 — three equivalent ways to drive the same internal `RwLock<bool>`)
//!
//! 1. Code: `handle.graph_pipeline().enable()` / `.disable()`
//! 2. Env: `LUNARIS_GRAPH_ENABLED=1` at `Lunaris::open(url)` time
//! 3. (Future) Config: `lunaris.config.graph.enabled = true` — Phase 5 OPS-08
//!    structured config lands the TOML reader; for v0 the env var IS the
//!    declarative knob.
//!
//! ## Lock discipline (CLAUDE.md "never hold a lock across `.await`")
//!
//! [`GraphPipelineHandle::snapshot_extractor`] clones the `Arc` out of the
//! read guard and returns it BEFORE any await. The ingest fan-out calls
//! `snapshot_extractor()` at the top of the graph-ON branch then awaits on
//! the returned `Arc` — the `RwLock` guard is dropped before the first
//! `.await` in the fan-out path.
//!
//! ## Idempotent observability (D-12)
//!
//! Toggle ON → OFF → ON emits exactly one `tracing::info!` per real
//! transition. Double-enable / double-disable emit zero events; the
//! `state_changes` counter only increments on actual flips. This makes the
//! v0 `tracing::field` surface noise-free; Phase 5 OPS-06 will swap the
//! counter to `prometheus::IntCounter` without changing the public API.

use std::sync::Arc;

use lunaris_core::LunarisError;
use lunaris_extract::{Extractor, NoopExtractor};
use parking_lot::{Mutex, RwLock};

/// Process-env knob (D-10) — `LUNARIS_GRAPH_ENABLED=1|true|on` flips the
/// initial state of [`GraphPipelineHandle`] at [`crate::Lunaris::open`] time.
/// Anything else (including unset) keeps the pipeline OFF (blueprint §5.2
/// default).
pub const ENABLED_ENV_VAR: &str = "LUNARIS_GRAPH_ENABLED";

/// The single switch (D-10). Default state is OFF unless
/// `LUNARIS_GRAPH_ENABLED=1` is set in env at construction time.
///
/// The handle owns BOTH the toggle bit AND the [`Arc<dyn Extractor>`] so the
/// ingest fan-out only needs to talk to one place to (a) check whether to
/// branch into graph-ON, and (b) snapshot the extractor for the actual
/// extract call.
pub struct GraphPipelineHandle {
    /// Toggle bit. Read on every `Lunaris::ingest` call; written by
    /// [`Self::enable`] / [`Self::disable`] — both idempotent.
    enabled: RwLock<bool>,
    /// Extractor handle. Stored under [`RwLock`] so [`Self::set_extractor`]
    /// and [`Self::force_reload`] can swap it at runtime without rebuilding
    /// the umbrella [`crate::Lunaris`] handle. Snapshotted via
    /// [`Self::snapshot_extractor`] which clones the `Arc` and drops the
    /// guard BEFORE the caller awaits — CLAUDE.md lock discipline.
    extractor: RwLock<Option<Arc<dyn Extractor>>>,
    /// State-change counter — incremented on every `enable()` AND `disable()`
    /// that flips the bool. Phase 5 OPS-06 will expose this via
    /// `prometheus::IntCounter` without changing the public API.
    state_changes: Mutex<u64>,
}

impl GraphPipelineHandle {
    /// Construct a fresh handle. `initial_enabled` is read from
    /// `LUNARIS_GRAPH_ENABLED=1|0` by [`crate::Lunaris::open`] (D-10);
    /// `extractor` is the one constructed by `default_extractor()` (a remote
    /// `CloudApiExtractor` when `LUNARIS_EXTRACT_PROVIDER` is set, else
    /// [`NoopExtractor`] — llama.cpp cutover, remote-only).
    pub fn new(initial_enabled: bool, extractor: Arc<dyn Extractor>) -> Self {
        Self {
            enabled: RwLock::new(initial_enabled),
            extractor: RwLock::new(Some(extractor)),
            state_changes: Mutex::new(0),
        }
    }

    /// Pure decision function — given a value (`None` = unset env var), return
    /// the toggle state. `"1"` / `"true"` / `"TRUE"` / `"on"` / `"ON"` → on;
    /// anything else → off.
    ///
    /// **B-1 fix:** this is a PURE function so tests pass `Some("1")` / `None`
    /// explicitly without mutating process env. Edition 2024 +
    /// `#![forbid(unsafe_code)]` makes [`std::env::set_var`] / `remove_var`
    /// `unsafe`; the pure fn dodges this entirely AND the parallel-test race
    /// where one test's `set_var` leaks into another's read.
    pub fn initial_state_from_value(raw: Option<&str>) -> bool {
        matches!(raw, Some("1" | "true" | "TRUE" | "on" | "ON"))
    }

    /// Convenience wrapper that reads [`ENABLED_ENV_VAR`] from process env
    /// and delegates to [`Self::initial_state_from_value`]. ONLY called by
    /// [`crate::Lunaris::open`] at construction time — NEVER from tests.
    /// Tests use [`Self::initial_state_from_value`] directly with explicit
    /// `Option` values.
    pub fn initial_state_from_env() -> bool {
        Self::initial_state_from_value(std::env::var(ENABLED_ENV_VAR).ok().as_deref())
    }

    /// Turn graph extraction ON. Idempotent — re-enabling reuses the existing
    /// loaded extractor unless [`Self::force_reload`] / [`Self::set_extractor`]
    /// is called explicitly.
    ///
    /// Emits `tracing::info!(state = "enabled", "graph_pipeline_state_changed")`
    /// only on a real state transition (D-12). The state_change counter only
    /// increments on actual flips so noise-free observability holds.
    pub fn enable(&self) {
        let mut w = self.enabled.write();
        if !*w {
            *w = true;
            *self.state_changes.lock() += 1;
            tracing::info!(state = "enabled", "graph_pipeline_state_changed");
        }
    }

    /// Turn graph extraction OFF. Idempotent — see [`Self::enable`] for
    /// observability semantics.
    pub fn disable(&self) {
        let mut w = self.enabled.write();
        if *w {
            *w = false;
            *self.state_changes.lock() += 1;
            tracing::info!(state = "disabled", "graph_pipeline_state_changed");
        }
    }

    /// Current toggle state — `true` if graph extraction will run on the next
    /// [`crate::Lunaris::ingest`] call.
    pub fn is_enabled(&self) -> bool {
        *self.enabled.read()
    }

    /// Total number of real state transitions since handle construction.
    /// Test harnesses assert this to verify D-12 (idempotent toggle
    /// observability). Production callers may surface it via Phase 5 OPS-06.
    pub fn state_change_count(&self) -> u64 {
        *self.state_changes.lock()
    }

    /// Force-reload hook (D-12). With the llama.cpp cutover the extractor is
    /// remote-only — there is no local weight cache to reload — so this is a
    /// no-op that returns `Ok(())` (callers don't need cfg-gated branches).
    /// Swap backends at runtime via [`Self::set_extractor`] instead.
    pub async fn force_reload(&self) -> Result<(), LunarisError> {
        tracing::info!("graph_pipeline_extractor_reloaded (noop — extractor is remote-only)");
        Ok(())
    }

    /// Replace the extractor on this handle. Test seam + production hook for
    /// custom backends (e.g., a tenant-specific `CloudApiExtractor` under the
    /// `cloud-api` feature) that callers want to install AFTER
    /// [`crate::Lunaris::open`].
    ///
    /// The toggle state is preserved — only the extractor `Arc` is swapped.
    pub fn set_extractor(&self, extractor: Arc<dyn Extractor>) {
        *self.extractor.write() = Some(extractor);
        tracing::info!("graph_pipeline_extractor_replaced");
    }

    /// CLAUDE.md "never hold a lock across `.await`": clone the `Arc` out of
    /// the read guard and return it. The guard is dropped at function return,
    /// BEFORE the caller awaits on the extractor.
    ///
    /// Returns `None` only when [`Self::set_extractor`] was passed an
    /// explicitly-cleared handle (not exposed in the public surface — the
    /// constructor always installs at least [`NoopExtractor`]).
    pub fn snapshot_extractor(&self) -> Option<Arc<dyn Extractor>> {
        self.extractor.read().clone()
    }

    /// Convenience installer for the default Noop extractor — used by the
    /// `with_parts` / `with_parts_keyword` test seams that don't pull the
    /// candle stack. Production callers go through [`crate::Lunaris::open`]
    /// which calls `default_extractor()`.
    pub fn with_noop() -> Self {
        Self::new(false, Arc::new(NoopExtractor) as Arc<dyn Extractor>)
    }
}

impl std::fmt::Debug for GraphPipelineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphPipelineHandle")
            .field("enabled", &*self.enabled.read())
            .field("has_extractor", &self.extractor.read().is_some())
            .field("state_change_count", &*self.state_changes.lock())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // B-1 fix: ALL tests use the pure `initial_state_from_value` function
    // with explicit `Option` values. NO test calls `std::env::set_var` or
    // `std::env::remove_var` — Edition 2024 + `#![forbid(unsafe_code)]`
    // makes both `unsafe`, AND parallel test execution would race on the
    // process env anyway.

    #[test]
    fn default_state_is_off_when_value_none() {
        assert!(!GraphPipelineHandle::initial_state_from_value(None));
    }

    #[test]
    fn value_one_enables_initial_state() {
        assert!(GraphPipelineHandle::initial_state_from_value(Some("1")));
        assert!(GraphPipelineHandle::initial_state_from_value(Some("true")));
        assert!(GraphPipelineHandle::initial_state_from_value(Some("TRUE")));
        assert!(GraphPipelineHandle::initial_state_from_value(Some("on")));
        assert!(GraphPipelineHandle::initial_state_from_value(Some("ON")));
    }

    #[test]
    fn value_off_disables_initial_state() {
        assert!(!GraphPipelineHandle::initial_state_from_value(Some("0")));
        assert!(!GraphPipelineHandle::initial_state_from_value(Some("false")));
        assert!(!GraphPipelineHandle::initial_state_from_value(Some("")));
        assert!(!GraphPipelineHandle::initial_state_from_value(Some("yes"))); // not in allowlist
        assert!(!GraphPipelineHandle::initial_state_from_value(Some("True"))); // mixed-case not in allowlist
    }

    #[test]
    fn enable_disable_is_observable_and_idempotent() {
        let h = GraphPipelineHandle::with_noop();
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

        // ON → OFF → ON sequence (D-12 verbatim) — three real transitions.
        h.enable();
        assert!(h.is_enabled());
        assert_eq!(h.state_change_count(), 3);
    }

    #[test]
    fn snapshot_extractor_returns_arc_clone() {
        let h = GraphPipelineHandle::new(true, Arc::new(NoopExtractor));
        let snap1 = h.snapshot_extractor();
        let snap2 = h.snapshot_extractor();
        assert!(snap1.is_some());
        assert!(snap2.is_some());
        // Two snapshots → same underlying `Arc` allocation (clone of one Arc).
        assert!(Arc::ptr_eq(snap1.as_ref().unwrap(), snap2.as_ref().unwrap()));
    }

    #[tokio::test]
    async fn force_reload_without_candle_is_noop() {
        // Without the `candle` feature this returns `Ok(())` infallibly so
        // callers don't need cfg-gated branches. Under the `candle` feature
        // it will attempt a real model load — that test path is covered by
        // the live-backend integration suite (extractor-it) in lunaris-extract.
        let h = GraphPipelineHandle::with_noop();
        // We can't assert the real-candle behavior from inside this unit test
        // (it would require ~3 GiB of weights present on disk). What we CAN
        // assert is that the noop path doesn't panic and doesn't change the
        // toggle state.
        let before = h.is_enabled();
        let _ = h.force_reload().await; // may Ok or Err depending on cache state
        assert_eq!(h.is_enabled(), before, "force_reload MUST NOT change toggle state");
    }

    #[test]
    fn set_extractor_replaces_handle() {
        let h = GraphPipelineHandle::with_noop();
        let original = h.snapshot_extractor().unwrap();

        // Insert a different `NoopExtractor` instance — Arc::ptr_eq must
        // distinguish them.
        let replacement: Arc<dyn Extractor> = Arc::new(NoopExtractor);
        h.set_extractor(replacement.clone());

        let after = h.snapshot_extractor().unwrap();
        assert!(!Arc::ptr_eq(&original, &after), "set_extractor must swap the Arc");
        assert!(Arc::ptr_eq(&replacement, &after));
    }

    #[test]
    fn set_extractor_preserves_toggle_state_and_counter() {
        let h = GraphPipelineHandle::with_noop();
        h.enable();
        assert_eq!(h.state_change_count(), 1);
        assert!(h.is_enabled());

        let replacement: Arc<dyn Extractor> = Arc::new(NoopExtractor);
        h.set_extractor(replacement);

        // set_extractor MUST NOT touch the toggle state or the change counter
        // — those are independent state. D-12 idempotent observability.
        assert!(h.is_enabled(), "set_extractor must not flip the toggle");
        assert_eq!(h.state_change_count(), 1, "set_extractor must not increment state changes");
    }

    #[test]
    fn debug_impl_is_safe_to_format() {
        let h = GraphPipelineHandle::with_noop();
        h.enable();
        let dbg = format!("{:?}", h);
        // Surface the three field names the Debug impl exposes — operators
        // grep these out of tracing logs.
        assert!(dbg.contains("enabled"));
        assert!(dbg.contains("has_extractor"));
        assert!(dbg.contains("state_change_count"));
    }
}
