//! Plan 05-01 — `POST /v1/recall` handler stub (PROTO-02/03 + D-04/05).
//!
//! Body for Task 1 is a B-7 stub; Task 3 wires the full SSE / JSON dispatch.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

pub async fn recall_handler(
    State(_state): State<AppState>,
    Json(_body): Json<serde_json::Value>,
) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({"error":"not_implemented","message":"task 3 placeholder"})),
    )
        .into_response()
}
