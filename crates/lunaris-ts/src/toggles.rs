#![allow(dead_code)]
//! Plan 08-03 BIND-TS-04 — three-surface pipeline toggles (code + env + config).
//!
//! The codegen-emitted [`crate::GraphPipelineHandle`] +
//! [`crate::ConsolidatorPipelineHandle`] classes in `generated.rs` expose
//! the single `toggle(on: bool)` entry (the canonical single-item binding
//! surface per Plan 08-01). This module layers ergonomic `enable()` /
//! `disable()` / `isEnabled()` / `initialStateFromEnv()` accessors that
//! TypeScript callers reach via `handle.graphPipeline.enable()` — the
//! code-surface path from `<key_constraints>`.
//!
//! ## Three surfaces
//!
//! 1. **Code**: `handle.graphPipeline.enable()` — calls
//!    [`::lunaris::GraphPipelineHandle::enable`] directly; no env or
//!    config involved. Immediate.
//! 2. **Env**: `LUNARIS_GRAPH_ENABLED=1` at `lunaris.open(url)` time — the
//!    Rust-side `Lunaris::open` already reads the env via
//!    [`::lunaris::GraphPipelineHandle::initial_state_from_env`], so the
//!    TS wrapper inherits this for free.
//! 3. **Config object**: `await open(url, { graphPipeline: { enabled: true }})`
//!    — handled by the TS-side `index.mts` wrapper, which walks the object
//!    AFTER construction and calls `handle.graphPipeline.enable()` when
//!    `enabled=true`.
//!
//! Surface-precedence resolution (code > env > config) is enforced by the
//! TS wrapper: the wrapper always calls `enable()` when the config says so,
//! overriding env-initial state. Code surfaces run AFTER the wrapper
//! returns, so they are always the final state.
//!
//! ## Exposed free helpers
//!
//! [`from_env`] / [`from_config`] helpers read the three-surface inputs
//! and return the final (bool, bool) tuple that the TS wrapper applies.
//! They exist so tests can assert the resolution order without
//! constructing a real handle.

use std::sync::Arc;

#[allow(unused_imports)]
use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::{ConsolidatorPipelineHandle, GraphPipelineHandle, Lunaris};

// =====================================================================
// Ergonomic methods on the codegen-frozen class wrappers.
//
// A second `#[napi] impl Lunaris` block would be allowed by napi-rs 3.x,
// but we keep the extra methods as free functions so the surface drift
// vs. the Python side (which MUST use free `#[pyfunction]` helpers to
// avoid the `multiple-pymethods` feature) stays minimal.
// =====================================================================

/// Return a `GraphPipelineHandle` wrapping the inner Rust handle.
/// TS callers reach this via `handle.graphPipeline` through the
/// `index.mts` getter installed on the class prototype.
#[napi(js_name = "getGraphPipeline")]
pub fn get_graph_pipeline(handle: &Lunaris) -> GraphPipelineHandle {
    let inner: Arc<::lunaris::GraphPipelineHandle> = handle.inner.graph_pipeline();
    GraphPipelineHandle { inner }
}

/// Return a `ConsolidatorPipelineHandle` wrapping the inner Rust handle.
#[napi(js_name = "getConsolidatorPipeline")]
pub fn get_consolidator_pipeline(handle: &Lunaris) -> ConsolidatorPipelineHandle {
    let inner: Arc<::lunaris::ConsolidatorPipelineHandle> = handle.inner.consolidator_pipeline();
    ConsolidatorPipelineHandle { inner }
}

#[napi(js_name = "graphPipelineEnable")]
pub fn graph_pipeline_enable(handle: &GraphPipelineHandle) {
    handle.inner.enable();
}

#[napi(js_name = "graphPipelineDisable")]
pub fn graph_pipeline_disable(handle: &GraphPipelineHandle) {
    handle.inner.disable();
}

#[napi(js_name = "graphPipelineIsEnabled")]
pub fn graph_pipeline_is_enabled(handle: &GraphPipelineHandle) -> bool {
    handle.inner.is_enabled()
}

#[napi(js_name = "consolidatorPipelineEnable")]
pub fn consolidator_pipeline_enable(handle: &ConsolidatorPipelineHandle) {
    handle.inner.enable();
}

#[napi(js_name = "consolidatorPipelineDisable")]
pub fn consolidator_pipeline_disable(handle: &ConsolidatorPipelineHandle) {
    handle.inner.disable();
}

#[napi(js_name = "consolidatorPipelineIsEnabled")]
pub fn consolidator_pipeline_is_enabled(handle: &ConsolidatorPipelineHandle) -> bool {
    handle.inner.is_enabled()
}

/// Read the Rust-side env initial-state reader. Exposed so tests can
/// assert that the env surface is wired through to the underlying
/// handle reader without having to construct a real `Lunaris` handle.
/// Returns `[graph_enabled, consolidator_enabled]` — napi-rs 3.x maps
/// `(bool, bool)` to a 2-element TS array.
///
/// NOTE: this reads `std::env::var(...)` on the Rust side, which bypasses
/// vitest's `process.env` Proxy layer — vitest-managed env mutations in
/// JS-land are NOT visible to `std::env::var` in the Rust side during
/// the same worker. Tests that want to exercise the env-parsing rules
/// should call [`from_env_value`] below with an explicit `Option<String>`.
#[napi(js_name = "fromEnv")]
pub fn from_env() -> Vec<bool> {
    let g = ::lunaris::GraphPipelineHandle::initial_state_from_env();
    let c = ::lunaris::ConsolidatorPipelineHandle::initial_state_from_env();
    vec![g, c]
}

/// Pure env-value decoder — mirrors Rust's
/// `GraphPipelineHandle::initial_state_from_value(Option<&str>)`. Tests
/// use this to assert the env-parsing rules without mutating the process
/// env (which vitest's `process.env` Proxy doesn't propagate to
/// `std::env::var` on the Rust side).
///
/// Accepts the RAW env-variable value (e.g. "1", "true", "on" → true;
/// anything else / null → false). Returns `[graph_enabled, consolidator_enabled]`
/// where both use the same parse rule.
#[napi(js_name = "fromEnvValue")]
pub fn from_env_value(graph_val: Option<String>, consolidator_val: Option<String>) -> Vec<bool> {
    let g = ::lunaris::GraphPipelineHandle::initial_state_from_value(graph_val.as_deref());
    let c = ::lunaris::ConsolidatorPipelineHandle::initial_state_from_value(
        consolidator_val.as_deref(),
    );
    vec![g, c]
}

/// Pure decision helper mirroring the Rust-side initial-state-from-env
/// readers but for a JS config object. Takes an opaque object of shape
/// `{ graphPipeline: { enabled: bool }, consolidatorPipeline: { enabled: bool } }`
/// and returns `[graphEnabled, consolidatorEnabled]`. Missing keys default
/// to `false` (default OFF per blueprint §5.1/§5.2).
///
/// Accepts both camelCase (`graphPipeline`) and snake_case
/// (`graph_pipeline`) keys — TS-side convention is camelCase but the
/// Python-side config dict uses snake_case and some callers share the
/// same config file across both bindings.
#[napi(js_name = "fromConfig")]
pub fn from_config(config: serde_json::Value) -> Vec<bool> {
    let g = read_nested_enabled(&config, "graphPipeline")
        || read_nested_enabled(&config, "graph_pipeline");
    let c = read_nested_enabled(&config, "consolidatorPipeline")
        || read_nested_enabled(&config, "consolidator_pipeline");
    vec![g, c]
}

fn read_nested_enabled(v: &serde_json::Value, key: &str) -> bool {
    v.get(key).and_then(|nested| nested.get("enabled")).and_then(|b| b.as_bool()).unwrap_or(false)
}
