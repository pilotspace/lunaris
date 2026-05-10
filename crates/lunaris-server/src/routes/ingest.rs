//! Plan 05-01 — `POST /v1/ingest` handler (PROTO-01 + D-03).
//!
//! Body: serde-deserialized `lunaris_core::Episode`.
//! Response: `IngestResponse { lsn, queue_lag_warn }` — D-03 verbatim shape.
//!
//! Single-atomic-write invariant preserved (INGEST-04). The handler proxies
//! `Lunaris::ingest` directly with NO new atomic boundary at the HTTP layer.
//!
//! ## Plan 05-05 W-11 fix — Extension<AuthClaims> + metric instrumentation
//!
//! Handler signature now extracts `AuthClaims` from request extensions
//! (populated by `auth_middleware`) so the `tenant` Prometheus label resolves
//! without a placeholder helper. The `lunaris_ingest_total{tenant,status}`
//! counter + `lunaris_ingest_duration_seconds{tenant}` histogram are
//! observed fire-and-forget per Shared Pattern 3 (PATTERNS.md) — never
//! block the request path on a metrics-recording failure.

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use lunaris_core::Episode;

use crate::dto::IngestResponse;
use crate::metrics::metrics;
use crate::middleware::auth::AuthClaims;
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
    Extension(claims): Extension<AuthClaims>,
    Json(episode): Json<Episode>,
) -> Response {
    let storage = state.lunaris.storage();
    let tenant = claims.scope.as_str();

    // Plan 05-05 OPS-06 — start the duration timer. Histogram observes
    // wall-clock from request entry to response sent.
    let timer = metrics().ingest_duration.with_label_values(&[tenant]).start_timer();

    // Single-atomic-write invariant preserved (INGEST-04). NO new atomic
    // boundary at the HTTP layer.
    let result = state.lunaris.ingest(episode).await;

    // Always observe the duration before mapping the result so error paths
    // also contribute to latency measurements.
    timer.observe_duration();

    let status = if result.is_ok() { "ok" } else { "error" };
    metrics().ingest_total.with_label_values(&[tenant, status]).inc();

    match result {
        Ok(lsn) => {
            // Phase 4 backpressure: read queue_depth and surface
            // queue_lag_warn. NotSupported / errors → no warning (best-effort).
            // RFC 0001 Wave 0: Scope::dev() until per-scope queue routing (Wave 1E).
            let warn = match storage.queue_depth(&lunaris_core::Scope::dev(), VERIFY_TOPIC, 0).await
            {
                Ok(d) => d > VERIFY_WARN_THRESHOLD,
                Err(_) => false,
            };
            (StatusCode::OK, Json(IngestResponse { lsn, queue_lag_warn: warn })).into_response()
        }
        Err(e) => map_error(e),
    }
}
