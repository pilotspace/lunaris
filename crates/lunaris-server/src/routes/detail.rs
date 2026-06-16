//! Memory Inspector (Phase 1) — single-primitive detail with resolved
//! provenance.
//!
//! `GET /v1/detail/{kind}/{id}` (TASK `detail-provenance` §3, FROZEN @ v1):
//!
//! For `kind ∈ episode|chunk|fact|community` it reads the KV row at
//! `{kind}_key(claims.scope, ulid)` and returns
//! `{ kind, id, primitive, provenance }`. `primitive` is the at-rest JSON
//! verbatim (lenient decode — a malformed row degrades to a lossy string, never
//! a 500, so a reviewer can SEE it). `provenance` resolves the upstream
//! observation per kind: chunk → the source episode at `chunk.episode_id`;
//! fact → the source episode at `source_episode_id` (when present) plus
//! `confidence` and `entities` (subject/object EntityIds as 32-char hex);
//! episode/community → no source episode (an episode IS the source; v0.3 stores
//! no community provenance).
//!
//! For `kind ∈ entity|relation` it returns `200 { graph_native: true }` with NO
//! storage read — these live in the graph (served by `graph-endpoint`); the SPA
//! routes them to `GET /v1/graph`.
//!
//! ## Design-for-failure — provenance resolution never cascades
//!
//! Only the PRIMITIVE read drives the status (200/404/500). Source-episode
//! resolution is best-effort: a dangling ref (`read_as_of → None`) is omitted
//! silently (a missing ref is normal data), and a backend error on a provenance
//! read flips `provenance.partial = true` rather than turning the primitive's
//! 200 into a 500. IO timeouts/retries are owned by the `StoragePort` driver,
//! same layering as `episode_handler`.

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use lunaris_core::keyspace::{chunk_key, community_key, episode_key, fact_key};
use lunaris_core::{Hlc, LunarisError, Scope, StoragePort};
use lunaris_extract::types::EntityId;
use ulid::Ulid;

use crate::middleware::auth::AuthClaims;
use crate::middleware::error::map_error;
use crate::state::AppState;

/// Handler for `GET /v1/detail/{kind}/{id}`.
///
/// Resolution order (per the frozen §3): (1) entity/relation → graph-native
/// signal; (2) unknown kind → 400 `invalid_kind`; (3) malformed id → 400
/// `invalid_id`; (4) primitive `read_as_of` → `None` = 404 / `Err` = 500;
/// (5) best-effort, non-cascading provenance resolution.
pub async fn detail_handler(
    State(state): State<AppState>,
    Extension(claims): Extension<AuthClaims>,
    Path((kind, id_str)): Path<(String, String)>,
) -> Response {
    let scope = &claims.scope;

    // (1) Graph-native kinds: entities are GraphNodes and relations are
    //     GraphEdges (NOT KV rows). Return the signal with NO storage read so
    //     the SPA routes to GET /v1/graph (graph-endpoint task).
    if kind == "entity" || kind == "relation" {
        return (
            StatusCode::OK,
            Json(serde_json::json!({ "kind": kind, "id": id_str, "graph_native": true })),
        )
            .into_response();
    }

    // (2) Validate the kind BEFORE parsing the id, so an unknown kind is
    //     `invalid_kind` regardless of id validity (no read performed).
    if !matches!(kind.as_str(), "episode" | "chunk" | "fact" | "community") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_kind",
                "message": format!(
                    "'{kind}' is not a detailable kind; \
                     expected one of episode|chunk|fact|community|entity|relation"
                ),
            })),
        )
            .into_response();
    }

    // (3) Parse the ULID path parameter.
    let ulid = match Ulid::from_string(&id_str) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_id",
                    "message": format!(
                        "'{id_str}' is not a valid ULID; \
                         expected a 26-character Crockford base-32 string"
                    ),
                })),
            )
                .into_response();
        }
    };

    // (4) Read the primitive row in the caller's JWT-bound scope.
    let as_of = state.lunaris.clock().tick();
    let storage = state.lunaris.storage();
    let key = match kind.as_str() {
        "episode" => episode_key(scope, ulid),
        "chunk" => chunk_key(scope, ulid),
        "fact" => fact_key(scope, ulid),
        "community" => community_key(scope, ulid),
        // Unreachable: step (2) already rejected every other kind.
        _ => unreachable!("kind validated to a KV kind above"),
    };

    let row = match storage.read_as_of(scope, &key, as_of).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "not_found",
                    "message": format!("no {kind} {ulid} in this scope"),
                })),
            )
                .into_response();
        }
        Err(e) => return map_error(LunarisError::Storage(e)),
    };

    // Decode the primitive verbatim — lenient, mirroring `episode_handler`: a
    // non-JSON value degrades to a lossy string rather than failing the request.
    let primitive: Value = serde_json::from_slice(&row.value)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&row.value).to_string()));

    // (5) Best-effort, NON-CASCADING provenance resolution.
    let provenance = resolve_provenance(storage.as_ref(), scope, &kind, &primitive, as_of).await;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "kind": kind,
            "id": id_str,
            "primitive": primitive,
            "provenance": provenance,
        })),
    )
        .into_response()
}

/// Resolve the `provenance` block for a primitive: the source episode(s),
/// `confidence` (fact only), and `entities` (fact subject/object as hex). A
/// provenance read error degrades to `partial: true` and is never propagated
/// as a 500 — the primitive already resolved.
async fn resolve_provenance(
    storage: &dyn StoragePort,
    scope: &Scope,
    kind: &str,
    primitive: &Value,
    as_of: Hlc,
) -> Value {
    let mut source_ids: Vec<Ulid> = Vec::new();
    let mut confidence: Value = Value::Null;
    let mut entities: Vec<String> = Vec::new();

    match kind {
        "chunk" => {
            // The chunk's source episode is `chunk.episode_id` (ULID string).
            if let Some(eid) = primitive.get("episode_id").and_then(Value::as_str)
                && let Ok(u) = Ulid::from_string(eid)
            {
                source_ids.push(u);
            }
        }
        "fact" => {
            // `confidence` passes through verbatim; `source_episode_id` is the
            // episode-level provenance written by `structured_ingest` (absent on
            // the pure-extractor path → no source episode, which is fine).
            confidence = primitive.get("confidence").cloned().unwrap_or(Value::Null);
            if let Some(s) = primitive.get("source_episode_id").and_then(Value::as_str)
                && let Ok(u) = Ulid::from_string(s)
            {
                source_ids.push(u);
            }
            // subject_id/object_id are stored as the raw `[u8;16]` array — the
            // exact shape `EntityId` deserializes from — so `Display` renders
            // the canonical 32-char lowercase hex.
            for field in ["subject_id", "object_id"] {
                if let Some(v) = primitive.get(field)
                    && let Ok(e) = serde_json::from_value::<EntityId>(v.clone())
                {
                    entities.push(e.to_string());
                }
            }
        }
        // episode / community → no source-episode provenance in v0.3.
        _ => {}
    }

    // Resolve each source episode. Best-effort + non-cascading: a `None` is a
    // dangling ref (omitted, NOT a fault); an `Err` flips `partial` but never
    // 500s the already-resolved primitive.
    let mut source_episodes: Vec<Value> = Vec::new();
    let mut partial = false;
    for sid in source_ids {
        let ekey = episode_key(scope, sid);
        match storage.read_as_of(scope, &ekey, as_of).await {
            Ok(Some(r)) => {
                let v: Value = serde_json::from_slice(&r.value).unwrap_or_else(|_| {
                    Value::String(String::from_utf8_lossy(&r.value).to_string())
                });
                source_episodes.push(v);
            }
            Ok(None) => {}            // dangling ref — normal data, not a fault
            Err(_) => partial = true, // degrade, never cascade
        }
    }

    serde_json::json!({
        "source_episodes": source_episodes,
        "confidence": confidence,
        "entities": entities,
        "partial": partial,
    })
}
