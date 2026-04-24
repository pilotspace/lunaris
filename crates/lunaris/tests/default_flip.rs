//! Plan 16-01 — default-flip regression tests.
//!
//! Four assertions on the default-backend resolution in
//! `ConsolidatorPipelineHandle` (CONSOL-V1-01):
//!
//! 1. Default (env unset) wires `ActRConsolidator`.
//! 2. `LUNARIS_CONSOLIDATOR_BACKEND=noop` opts out to `NoopConsolidator`.
//! 3. `enable_for_scope("helios:fs/")` wiring is preserved verbatim after the flip.
//! 4. `set_consolidator(Arc::new(NoopConsolidator))` code-level override still works.
//!
//! Env-mutating tests serialize through a module-local `parking_lot::Mutex`
//! because the crate does not carry `serial_test` as a dev-dep and we MUST NOT
//! hold it across an `.await` (CLAUDE.md lock discipline) — each guard drops
//! before any `.await` via an explicit scope + `drop(guard)`.
//!
//! Path deviation: plan spec put this file under
//! `crates/lunaris-consolidate/tests/default_flip.rs`, but the resolver +
//! `ConsolidatorPipelineHandle` live in the `lunaris` crate and
//! `lunaris-consolidate` cannot reverse-depend on `lunaris`. Moved here as a
//! Rule 3 (blocking-issue) scope correction.

// `std::env::{set_var,remove_var}` are `unsafe` in Rust 2024 edition; the
// module-local `ENV_GUARD` serializes access within this test binary, and
// each guard drops before any `.await`. We therefore permit `unsafe` at the
// test-binary level ONLY — the production crate retains `#![forbid(unsafe_code)]`.
#![allow(unsafe_code)]

use std::sync::Arc;

use lunaris::{ConsolidatorPipelineHandle, NoopConsolidator};
use lunaris_consolidate::Consolidator;
use parking_lot::Mutex;

/// Global guard — prevents concurrent env mutation across the four tests in
/// this file. Never held across an `.await`.
static ENV_GUARD: Mutex<()> = Mutex::new(());

/// Scrub the env var before + after each test so no state leaks between tests
/// or between `cargo test` runs on the same shell.
fn with_clean_env<F: FnOnce()>(f: F) {
    let guard = ENV_GUARD.lock();
    // SAFETY-NOTE: `remove_var` / `set_var` are `unsafe` in Rust 2024. The
    // `parking_lot::Mutex` above serializes access within this test binary;
    // other test binaries in the same workspace are isolated by cargo's
    // one-binary-per-test-target model.
    unsafe { std::env::remove_var("LUNARIS_CONSOLIDATOR_BACKEND") };
    f();
    unsafe { std::env::remove_var("LUNARIS_CONSOLIDATOR_BACKEND") };
    drop(guard);
}

/// Test 1 — default resolution is `ActRConsolidator` when the env var is unset.
#[test]
fn default_backend_is_actr() {
    with_clean_env(|| {
        let backend = ConsolidatorPipelineHandle::backend_from_env()
            .expect("unset env resolves Ok(ActRConsolidator)");
        assert_eq!(
            backend.debug_name(),
            "ActRConsolidator",
            "default (env unset) MUST resolve to ActRConsolidator per CONSOL-V1-01"
        );
    });
}

/// Test 2 — `LUNARIS_CONSOLIDATOR_BACKEND=noop` opts out to `NoopConsolidator`.
#[test]
fn env_noop_opts_out() {
    with_clean_env(|| {
        unsafe { std::env::set_var("LUNARIS_CONSOLIDATOR_BACKEND", "noop") };
        let backend = ConsolidatorPipelineHandle::backend_from_env()
            .expect("noop env value resolves Ok(NoopConsolidator)");
        assert_eq!(
            backend.debug_name(),
            "NoopConsolidator",
            "operator opt-out via env var MUST pin NoopConsolidator"
        );
    });
}

/// Test 3 — scope filter semantic is preserved across the flip.
#[test]
fn scope_filter_intact_after_flip() {
    with_clean_env(|| {
        let backend = ConsolidatorPipelineHandle::backend_from_env()
            .expect("default backend resolves");
        let h = ConsolidatorPipelineHandle::new(false, backend);
        h.enable_for_scope("helios:fs/");
        assert_eq!(
            h.scope_prefix(),
            Some("helios:fs/".to_string()),
            "enable_for_scope still installs the prefix after the default flip"
        );
    });
}

/// Test 4 — `set_consolidator` code-level override still works (third toggle
/// surface).
#[test]
fn set_consolidator_code_override_still_works() {
    with_clean_env(|| {
        let backend = ConsolidatorPipelineHandle::backend_from_env()
            .expect("default backend resolves");
        let h = ConsolidatorPipelineHandle::new(false, backend);
        assert_eq!(
            h.snapshot_consolidator().unwrap().debug_name(),
            "ActRConsolidator",
            "default seat is ActRConsolidator before override"
        );
        h.set_consolidator(Arc::new(NoopConsolidator) as Arc<dyn Consolidator>);
        assert_eq!(
            h.snapshot_consolidator().unwrap().debug_name(),
            "NoopConsolidator",
            "set_consolidator(Arc::new(NoopConsolidator)) pins the noop backend"
        );
    });
}

/// Test 5 — unknown env values fail-fast (not silent fallback).
#[test]
fn env_unknown_value_fails_fast() {
    with_clean_env(|| {
        unsafe { std::env::set_var("LUNARIS_CONSOLIDATOR_BACKEND", "ollama") };
        let result = ConsolidatorPipelineHandle::backend_from_env();
        let err = match result {
            Ok(_) => panic!("unknown backend values MUST NOT silently fall back"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("LUNARIS_CONSOLIDATOR_BACKEND"),
            "error must name the env var; got: {msg}"
        );
        assert!(
            msg.contains("ollama") || msg.contains("actr") || msg.contains("noop"),
            "error must be descriptive; got: {msg}"
        );
    });
}
