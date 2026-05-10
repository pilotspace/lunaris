//! `POST /v1/ingest` handler — RFC 0001 Wave 1E migration.
//!
//! ## Wave 1E scope routing (RFC 0001 §3.8)
//!
//! The handler now:
//! 1. Extracts the JWT-bound `AuthClaims.scope` (a typed `Scope`).
//! 2. Deserializes `IngestBody` — a DTO **without** a `scope` field, with
//!    `deny_unknown_fields` so a client cannot smuggle a `"scope"` key through.
//! 3. Constructs an `Episode` stamping the JWT-bound scope onto it.
//! 4. Calls `engine.scoped(claims.scope.clone()).ingest(episode)` — the
//!    `ScopedLunaris` wrapper enforces that the episode's scope matches the
//!    bound scope before forwarding to the underlying engine.
//!
//! This closes the silent-leak surface that existed in v0.1 where a client
//! could send `{"scope": "victim", ...}` and write into another agent's
//! partition. The `queue_depth` call also uses the real `claims.scope`
//! instead of the `Scope::dev()` crutch.
//!
//! ## Invariants preserved
//!
//! - INGEST-04: single-atomic-write invariant. No new atomic boundary at
//!   the HTTP layer — the `ScopedLunaris::ingest` path still issues exactly
//!   one `atomic_write` downstream.

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use lunaris_core::{BiTemporal, Episode};
use ulid::Ulid;

use crate::dto::{IngestBody, IngestResponse};
use crate::metrics::metrics;
use crate::middleware::auth::AuthClaims;
use crate::middleware::error::map_error;
use crate::state::AppState;

pub async fn ingest_handler(
    State(state): State<AppState>,
    Extension(claims): Extension<AuthClaims>,
    Json(body): Json<IngestBody>,
) -> Response {
    let scope_str = claims.scope.as_str();

    // RFC 0001 §3.8: bind a scoped view of the engine to the JWT-bound scope.
    // ScopedLunaris::ingest will override episode.scope with self.scope before
    // forwarding to the underlying engine — client-supplied scope is rejected
    // at the IngestBody deserialization step (deny_unknown_fields), and the
    // second enforcement point is the scope override inside ScopedLunaris::ingest.
    let scoped = state.lunaris.scoped(claims.scope.clone());

    // Plan 05-05 OPS-06 — start the duration timer.
    let timer = metrics().ingest_duration.with_label_values(&[scope_str]).start_timer();

    // Construct a fresh HlcClock tick for this episode's bitemporal stamp.
    // The Lunaris handle owns the canonical clock; we borrow it here.
    let clock = state.lunaris.clock();
    let bt = BiTemporal::now(&clock);
    let episode = Episode {
        id: body.id.unwrap_or_else(Ulid::new),
        scope: claims.scope.clone(), // Will be overridden by ScopedLunaris::ingest;
        // set here defensively for clarity.
        source: body.source,
        content: body.content,
        t_ref: body.t_ref,
        bt,
        metadata: body.metadata,
    };

    // INGEST-04: single atomic write, no new boundary at the HTTP layer.
    let result = scoped.ingest(episode).await;

    timer.observe_duration();

    let status = if result.is_ok() { "ok" } else { "error" };
    metrics().ingest_total.with_label_values(&[scope_str, status]).inc();

    match result {
        Ok(lsn) => {
            // RFC 0001 Wave 1E: use the real claims.scope for queue_depth
            // instead of the Scope::dev() crutch that Wave 0 introduced.
            let storage = state.lunaris.storage();
            let warn = match storage.queue_depth(&claims.scope, VERIFY_TOPIC, 0).await {
                Ok(d) => d > VERIFY_WARN_THRESHOLD,
                Err(_) => false,
            };
            (StatusCode::OK, Json(IngestResponse { lsn, queue_lag_warn: warn })).into_response()
        }
        Err(e) => map_error(e),
    }
}

/// Topic name mirrored from `lunaris_verify::worker::VERIFY_TOPIC`.
const VERIFY_TOPIC: &str = "__lunaris_verify__";

/// Threshold for `queue_lag_warn=true` in the response body.
const VERIFY_WARN_THRESHOLD: u64 = 1000;
