//! Plan 05-01 — `GET /healthz` (D-03). No auth, no rate-limit.
//!
//! Stub body for Task 1; Task 2 replaces with full version-emitting handler.

use axum::Json;
use axum::extract::State;
use serde_json::json;

use crate::state::AppState;

pub async fn healthz_handler(State(_state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
