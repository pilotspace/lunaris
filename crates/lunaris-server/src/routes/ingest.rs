//! Plan 05-01 — `POST /v1/ingest` handler (PROTO-01 + D-03).
//!
//! Body: serde-deserialized `lunaris_core::Episode`.
//! Response: `IngestResponse { lsn, queue_lag_warn }` — D-03 verbatim shape.
//!
//! Single-atomic-write invariant preserved (INGEST-04). The handler proxies
//! `Lunaris::ingest` directly with NO new atomic boundary at the HTTP layer.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use lunaris_core::Episode;

use crate::dto::IngestResponse;
use crate::middleware::error::map_error;
use crate::state::AppState;

/// Topic name mirrored from `lunaris_verify::worker::VERIFY_TOPIC`. Hard-coded
/// here so the lunaris-server crate doesn't need to depend on lunaris-verify
/// just for one string constant.
const VERIFY_TOPIC: &str = "__lunaris_verify__";

/// Threshold for `queue_lag_warn=true` in the response body. Matches the
/// `LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` default in
/// `crates/lunaris/src/recall.rs::DEFAULT_VERIFY_WARN_THRESHOLD`.
const VERIFY_WARN_THRESHOLD: u64 = 1000;

pub async fn ingest_handler(
    State(state): State<AppState>,
    Json(episode): Json<Episode>,
) -> Response {
    let storage = state.lunaris.storage();
    // Single-atomic-write invariant preserved (INGEST-04). NO new atomic
    // boundary at the HTTP layer.
    match state.lunaris.ingest(episode).await {
        Ok(lsn) => {
            // Phase 4 backpressure: read queue_depth and surface
            // queue_lag_warn. NotSupported / errors → no warning (best-effort).
            let warn = match storage.queue_depth(VERIFY_TOPIC, 0).await {
                Ok(d) => d > VERIFY_WARN_THRESHOLD,
                Err(_) => false,
            };
            (
                StatusCode::OK,
                Json(IngestResponse {
                    lsn,
                    queue_lag_warn: warn,
                }),
            )
                .into_response()
        }
        Err(e) => map_error(e),
    }
}
