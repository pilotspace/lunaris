#![allow(dead_code)]
//! Plan 08-03 Task 2 — TS DSL camelCase parity module.
//!
//! The codegen-emitted `RetrievalBuilder` in `generated.rs` freezes the
//! method signatures but leaves the owned-self builder-method bodies as
//! `Err(napi::Error::from_reason("Plan 08-03 wires the body"))` stubs
//! (owned-self consuming methods don't fit the napi-rs `&self` receiver
//! shape cleanly — a TS-side Promise has nothing to "consume"). Mirroring
//! Plan 08-02's PyO3 host, the real working retrieve DSL lives on the
//! TypeScript side in `index.mts`; this Rust module provides the one
//! FFI entry the TS-side terminal `.execute()` needs to collapse a plan
//! into a single recall round-trip.
//!
//! ## Exposed helpers
//!
//! - [`recall_simple_execute`] — `&Lunaris` + an opaque plan JSON →
//!   `Promise<Array<any>>` (hydrated hits). For v0.1.1 the accepted plan
//!   shape is the default `Vector("chunks", k).top(n).execute(Query::text(q))`
//!   path; richer operator trees land when a downstream vertical wrapper
//!   asks for them.
//! - [`open_lunaris`] — ergonomic free function so TS callers can do
//!   `await open(url)` without importing `Lunaris` (matches Plan 08-02's
//!   `open_handle`).

use std::sync::Arc;

#[allow(unused_imports)]
use napi::bindgen_prelude::*;
use napi_derive::napi;

use ::lunaris::{Query, Vector};

use crate::Lunaris;
use crate::errors::napi_err;

/// Minimal `recall` execute bridge for the TS-side DSL.
///
/// Accepts a plan JSON of shape:
/// `{"query": "...", "k": 5, "index": "chunks", "filter": "...", "as_of_ms": 123}`.
/// All keys are optional; missing fields fall back to
/// `{query: "", k: 5, index: "chunks"}`. Returns a JSON array of hit
/// objects which napi-rs 3.x converts to a `Promise<Array<object>>` in TS.
#[napi(js_name = "recallSimpleExecute")]
pub async fn recall_simple_execute(
    handle: &Lunaris,
    plan: serde_json::Value,
) -> napi::Result<serde_json::Value> {
    let inner: Arc<::lunaris::Lunaris> = handle.inner.clone();

    // Extract plan fields with safe defaults so the TS-side builder can
    // forward a sparse object.
    let query_text = plan.get("query").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let k = plan.get("k").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(5);
    let index = plan.get("index").and_then(|v| v.as_str()).unwrap_or("chunks").to_string();
    let filter_str_opt = plan.get("filter").and_then(|v| v.as_str()).map(|s| s.to_string());
    let as_of_ms = plan.get("as_of_ms").and_then(|v| v.as_u64());

    // Build the operator tree on the Rust side. The TS-side DSL builder
    // collapses more elaborate compositions (fuse_rrf, graph, as_of,
    // filter) into the same underlying Query via the filter+as_of+k knobs.
    let root = Vector::new(&index, k);
    let mut builder = inner.recall().with_root(root);
    if let Some(s) = filter_str_opt {
        builder = builder.filter_str(&s).map_err(|e| {
            crate::errors::napi_err_with_code("VALIDATE", format!("filter_str: {e}"))
        })?;
    }
    if let Some(ms) = as_of_ms {
        // `Hlc::from_parts(wall_ms, counter, node_id)` — the only public
        // non-ZERO constructor; counter=0 + node_id=0 is the standard
        // time-travel witness shape.
        let hlc = ::lunaris::Hlc::from_parts(ms, 0, 0);
        builder = builder.as_of(hlc);
    }
    let q = Query::text(&query_text);
    let hits = builder.execute(q).await.map_err(napi_err)?;
    let value = serde_json::to_value(&hits).map_err(napi_err)?;
    Ok(value)
}

/// Ergonomic async `open` free function so TypeScript callers can write
/// `await open(url)` without importing the `Lunaris` class directly —
/// matches Plan 08-02's `lunaris.open(url)` surface.
///
/// napi-rs 3.x exposes this as `openHandle(url: string): Promise<Lunaris>`
/// at the TS layer. The handwritten `index.mts` wrapper re-exports it as
/// `open(url, opts?)` with the three-surface config path.
#[napi(js_name = "openHandle")]
pub async fn open_lunaris(url: String) -> napi::Result<Lunaris> {
    let inner = ::lunaris::Lunaris::open(&url).await.map_err(napi_err)?;
    Ok(Lunaris { inner: Arc::new(inner) })
}
