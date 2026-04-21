//! Plan 05-01 — `POST /v1/ingest` handler (PROTO-01 + D-03).
//!
//! Body for Task 1 is a B-7 stub returning a plain JSON envelope so the lib::build
//! Router compiles; Task 2 replaces with the full Episode-deserializing
//! `Lunaris::ingest` proxy.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

pub async fn ingest_handler(
    State(_state): State<AppState>,
    Json(_body): Json<serde_json::Value>,
) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({"error":"not_implemented","message":"task 2 placeholder"})),
    )
        .into_response()
}
