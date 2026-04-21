//! Plan 05-01 — `GET /v1/snapshot/{lsn}` handler stub (PROTO-01 + D-03).
//!
//! Body for Task 1 is a B-7 stub; Task 2 wires the NDJSON streaming response
//! over `StoragePort::scan_range`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

pub async fn snapshot_handler(
    State(_state): State<AppState>,
    Path(_lsn): Path<String>,
) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({"error":"not_implemented","message":"task 2 placeholder"})),
    )
        .into_response()
}
