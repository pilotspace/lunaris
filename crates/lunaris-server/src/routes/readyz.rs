//! 0.6.2 P0-3 — `GET /readyz` (Kubernetes readinessProbe). No auth, no
//! rate-limit, same open surface as `/healthz`.
//!
//! `200 {ready:true, checks:{...}}` when storage PINGs, accepts the write
//! canary, and the embedder is configured; `503 {ready:false, ...}` otherwise.
//! The `checks` map names the failing component so an operator does not have to
//! guess which half of the stack is wedged.
//!
//! Kubernetes semantics, on purpose:
//! - **livenessProbe → `/healthz`**: is the process alive? Failing it restarts
//!   the pod.
//! - **readinessProbe → `/readyz`**: can the process actually serve? Failing it
//!   removes the pod from the LB without restarting it — the right answer when
//!   the wedge is downstream, since a restart would only add a cold start.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::json;

use crate::state::AppState;

pub async fn readyz_handler(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let version = env!("CARGO_PKG_VERSION");
    let report = state.readiness.check(&state.lunaris).await;
    let status = if report.ready { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    if !report.ready {
        tracing::warn!(checks = ?report.checks, "readyz: NOT ready -> 503");
    }
    (
        status,
        Json(json!({
            "ready": report.ready,
            "version": version,
            "checks": report.checks,
        })),
    )
}
