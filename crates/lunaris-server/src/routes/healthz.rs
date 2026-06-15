//! Plan 05-01 — `GET /healthz` (D-03). No auth, no rate-limit.
//!
//! observability-rollout-maturity: the handler now PROBES storage liveness via
//! [`Lunaris::health_check`] and answers **503** when the probe fails, so the
//! 5%→100% rollout auto-cutback can key on EITHER the HTTP status OR the
//! `{ok:false}` body. A healthy probe answers 200 `{ok:true, version}`.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::json;

use crate::state::AppState;

pub async fn healthz_handler(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let version = env!("CARGO_PKG_VERSION");
    match state.lunaris.health_check().await {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true, "version": version }))),
        Err(e) => {
            tracing::warn!(error = %e, "healthz storage probe failed -> 503");
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "ok": false, "version": version })))
        }
    }
}
