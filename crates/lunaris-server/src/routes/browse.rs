//! Memory Inspector (Phase 1) — query-less browse surface.
//!
//! Two read-only, JWT-scoped routes the dashboard binds to (TASK
//! `browse-endpoints` §3, FROZEN @ v1):
//!
//! - `GET /v1/scopes?prefix=&cursor=&limit=` — CROSS-SCOPE partition
//!   enumeration over [`lunaris::Lunaris::list_scopes`]. Deliberately NOT
//!   filtered by `claims.scope`: a `"recall"` token learns every partition
//!   NAME (not its data). Accepted for the Phase-1 Moon-native local dashboard;
//!   revisit for multi-tenant entitlement filtering (least-sure flag, §3).
//! - `GET /v1/browse/{kind}?cursor=&limit=` — paginate one scope's primitives
//!   of `kind ∈ episode|chunk|entity|relation|fact|community`, scoped to the
//!   JWT `claims.scope` ONLY (wire-side `scope` ignored). Dispatches
//!   `kind → keyspace::{kind}_prefix(scope) → scan_page::<T> → Page<T>`.
//!
//! ## Why typed `scan_page::<T>` (not `serde_json::Value`)
//!
//! The browse handler deserializes into the concrete primitive type so a
//! fact-prefixed row whose value is valid JSON but NOT a [`Fact`] surfaces as
//! `corrupt_row` (500), per the frozen contract — a `Value` target would accept
//! any JSON and silently hide the corruption. The deserialized `Page<T>.items`
//! are re-serialized to the full primitive JSON for the response body.
//!
//! ## Error envelope
//!
//! Reuses the `map_error` shape `{ "error": code, "message": msg }`. Browse
//! errors derive their code from [`ListError::code`]
//! (`invalid_limit|limit_too_large|invalid_cursor` → 400 ·
//! `corrupt_row|storage` → 500); `invalid_kind` is rejected pre-scan (400);
//! `/scopes` `NotSupported` → 501 via `map_error`.

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

use lunaris_core::keyspace::{chunk_prefix, community_prefix, episode_prefix, fact_prefix};
use lunaris_core::{Chunk, Community, Episode, ListError, Page, Scope, StoragePort, scan_page};
// The at-rest `fact:` KV row is a `lunaris_extract::Fact` (subject_id/object_id
// EntityIds, no scope/bt/provenance), NOT `lunaris_core::Fact`. Deserializing
// into the real type preserves the corrupt_row guarantee.
use lunaris_extract::types::Fact as ExtractFact;

use crate::dto::{BrowseQuery, ScopesQuery};
use crate::middleware::auth::AuthClaims;
use crate::middleware::error::map_error;
use crate::state::AppState;

/// Handler for `GET /v1/browse/{kind}`.
///
/// KV-backed kinds (`episode`/`chunk`/`community` → core primitives; `fact` →
/// `lunaris_extract::Fact`, the real at-rest shape) dispatch to the matching
/// keyspace prefix, page the caller's JWT-bound scope via [`scan_page`], and
/// serialize the typed `Page<T>` to `{ items, next_cursor }`. Graph-native
/// kinds (`entity`/`relation`) live in the graph, not KV — they return
/// `200 { items: [], next_cursor: null, graph_native: true }` with no scan, so
/// the SPA routes to `GET /v1/graph`. An unknown kind is `400 invalid_kind`.
pub async fn browse_handler(
    State(state): State<AppState>,
    Extension(claims): Extension<AuthClaims>,
    Path(kind): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> Response {
    let scope = &claims.scope;
    let storage = state.lunaris.storage();
    let cursor = q.cursor.as_deref();

    // Dispatch kind → (prefix, typed scan). Unknown kind is a pre-scan 400 so
    // no I/O occurs (scenario: "no scan is performed").
    let page = match kind.as_str() {
        "episode" => {
            page_json::<Episode>(storage.as_ref(), scope, &episode_prefix(scope), cursor, q.limit)
                .await
        }
        "chunk" => {
            page_json::<Chunk>(storage.as_ref(), scope, &chunk_prefix(scope), cursor, q.limit).await
        }
        // Graph-native kinds: entities are GraphNodes and relations are
        // GraphEdges (NOT KV rows), so there is nothing to scan. Return a typed
        // empty page carrying `graph_native: true` so the SPA routes to
        // GET /v1/graph instead of rendering an empty table as "no data".
        // No storage call is made (scenario: "no scan is performed").
        "entity" | "relation" => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "items": [],
                    "next_cursor": serde_json::Value::Null,
                    "graph_native": true,
                })),
            )
                .into_response();
        }
        // The at-rest fact row is `lunaris_extract::Fact`, not `core::Fact`.
        "fact" => {
            page_json::<ExtractFact>(storage.as_ref(), scope, &fact_prefix(scope), cursor, q.limit)
                .await
        }
        "community" => {
            page_json::<Community>(
                storage.as_ref(),
                scope,
                &community_prefix(scope),
                cursor,
                q.limit,
            )
            .await
        }
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_kind",
                    "message": format!(
                        "'{other}' is not a browsable kind; \
                         expected one of episode|chunk|entity|relation|fact|community"
                    ),
                })),
            )
                .into_response();
        }
    };

    match page {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(e) => list_error_response(&e),
    }
}

/// Handler for `GET /v1/scopes` — CROSS-SCOPE partition enumeration.
///
/// Auth is required (the `scoped_auth("recall")` layer) but `claims.scope` is
/// intentionally unused: this lists EVERY partition name the backend knows, not
/// just the caller's. Backends without enumeration (Postgres) surface
/// `NotSupported` → 501 via `map_error`.
pub async fn scopes_handler(
    State(state): State<AppState>,
    Extension(_claims): Extension<AuthClaims>,
    Query(q): Query<ScopesQuery>,
) -> Response {
    match state.lunaris.list_scopes(q.prefix.as_deref(), q.limit, q.cursor.as_deref()).await {
        // `ScopePage` is `Serialize` as `{ scopes, next_cursor }` — exactly the
        // frozen envelope, so we serialize it directly.
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(e) => map_error(e),
    }
}

/// Page `T` under `prefix` and serialize to the `{ items, next_cursor }`
/// envelope. `T` must round-trip serde so a non-`T` row fails as `corrupt_row`.
async fn page_json<T: DeserializeOwned + Serialize>(
    storage: &dyn StoragePort,
    scope: &Scope,
    prefix: &[u8],
    cursor: Option<&str>,
    limit: usize,
) -> Result<serde_json::Value, ListError> {
    // `as_of = None` — Phase-1 browse is current state only (Phase-2 timeline
    // deferred). Cursor/limit validation happens inside `scan_page` before I/O.
    let page: Page<T> = scan_page::<T>(storage, scope, prefix, cursor, limit, None).await?;
    Ok(serde_json::json!({ "items": page.items, "next_cursor": page.next_cursor }))
}

/// Map a [`ListError`] to the `{ error, message }` envelope with the contracted
/// status: validation rejects → 400, corrupt/backend → 500.
fn list_error_response(e: &ListError) -> Response {
    let status = match e {
        ListError::InvalidLimit | ListError::LimitTooLarge | ListError::InvalidCursor => {
            StatusCode::BAD_REQUEST
        }
        ListError::CorruptRow | ListError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(serde_json::json!({ "error": e.code(), "message": e.to_string() })))
        .into_response()
}
