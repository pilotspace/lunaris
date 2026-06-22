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

use lunaris::episode_builder::EpisodeBuilder;

use crate::dto::{
    CATEGORIES_METADATA_KEY, IngestBody, IngestResponse, LevelError, compose_request_scope,
    validate_categories,
};
use crate::metrics::metrics;
use crate::middleware::auth::AuthClaims;
use crate::middleware::error::map_error;
use crate::state::AppState;

/// Map a level/category rejection to a `400` with the contract's error code.
/// Shared with the recall handler (`crate::routes::recall`).
pub(crate) fn level_reject(e: LevelError) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": e.code(),
            "message": match e {
                LevelError::InvalidSegment =>
                    "level ids must be 1..=N chars of [A-Za-z0-9_-] (no '.'/':'/'/')",
                LevelError::ScopeTooLong =>
                    "composed scope exceeds the 128-byte cap (shorten tenant + level ids)",
                LevelError::InvalidCategories =>
                    "categories must be ≤16 items, each 1..=64 bytes",
            },
        })),
    )
        .into_response()
}

pub async fn ingest_handler(
    State(state): State<AppState>,
    Extension(claims): Extension<AuthClaims>,
    Json(body): Json<IngestBody>,
) -> Response {
    // Metrics stay labeled by the JWT BASE scope — composing the sub-level
    // into the label would unbound metric cardinality (one series per user /
    // session). The base scope is the right aggregation key.
    let scope_str = claims.scope.as_str();

    // multi-level-memory-categories: compose the optional level ids onto the
    // JWT base scope, and validate categories. Both reject with 400 BEFORE any
    // timer / storage work — a rejected ingest writes nothing.
    let scope = match compose_request_scope(
        &claims.scope,
        body.user_id.as_deref(),
        body.agent_id.as_deref(),
        body.session_id.as_deref(),
    ) {
        Ok(s) => s,
        Err(e) => return level_reject(e),
    };
    if let Err(e) = validate_categories(&body.categories) {
        return level_reject(e);
    }

    // RFC 0001 §3.8: bind a scoped view of the engine to the COMPOSED scope
    // (JWT base is always its prefix — a sub-partition, never an escape).
    // ScopedLunaris::ingest stamps self.scope onto the episode via
    // EpisodeBuilder::into_episode — callers cannot inject an arbitrary scope.
    // deny_unknown_fields on IngestBody rejects any "scope" key at the HTTP boundary.
    let scoped = state.lunaris.scoped(scope.clone());

    // Plan 05-05 OPS-06 — start the duration timer.
    let timer = metrics().ingest_duration.with_label_values(&[scope_str]).start_timer();

    // Assemble a scope-less EpisodeBuilder. Scope is stamped by ScopedLunaris::ingest.
    let mut builder = EpisodeBuilder::new(body.source, body.content);
    if let Some(id) = body.id {
        builder = builder.id(id);
    }
    if let Some(t_ref) = body.t_ref {
        builder = builder.t_ref(t_ref);
    }
    // Categories ride in Episode.metadata under the well-known key so the
    // recall-side Filter::Eq/Or bites on them (Moon FT TAG / PG metadata WHERE
    // / embedded json_each membership).
    let mut metadata = body.metadata;
    if !body.categories.is_empty() {
        metadata.insert(
            CATEGORIES_METADATA_KEY.to_string(),
            serde_json::Value::Array(
                body.categories.into_iter().map(serde_json::Value::String).collect(),
            ),
        );
    }
    if !metadata.is_empty() {
        builder = builder.metadata(metadata);
    }

    // INGEST-04: single atomic write, no new boundary at the HTTP layer.
    let result = scoped.ingest(builder).await;

    timer.observe_duration();

    let status = if result.is_ok() { "ok" } else { "error" };
    metrics().ingest_total.with_label_values(&[scope_str, status]).inc();

    match result {
        Ok(lsn) => {
            // Query queue_depth on the COMPOSED scope — that is where the
            // verify task was enqueued by the scoped ingest, so the warn
            // reflects the right partition's backpressure.
            let storage = state.lunaris.storage();
            let warn = match storage.queue_depth(&scope, VERIFY_TOPIC, 0).await {
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
