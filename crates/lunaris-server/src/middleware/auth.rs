//! Plan 05-01 — Bearer-token middleware (PROTO-05 + D-07).
//!
//! Token map loaded from `--tokens-file` JSON (D-07 verbatim shape):
//! ```json
//! { "<token>": { "tenant": "<id>", "scopes": ["ingest", "recall", "forget"] } }
//! ```
//!
//! Behavior:
//! - Missing / malformed `Authorization` header → 401 Unauthorized.
//! - Token not in map → 401 Unauthorized.
//! - Token present but lacks the `RequiredScope` for the route → 403 Forbidden.
//!
//! Authenticated requests get [`AuthClaims`] attached via
//! `Request::extensions_mut()` so downstream handlers + the rate-limit
//! middleware can read `tenant`.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// Per-request bearer-token claims attached by [`auth_middleware`].
#[derive(Clone, Debug)]
pub struct AuthClaims {
    pub tenant: String,
    pub scopes: Arc<Vec<String>>,
}

/// Required scope for the matched route. Set via `Router::route_layer`
/// per-route (see `lib::build`).
#[derive(Clone, Debug)]
pub struct RequiredScope(pub &'static str);

pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let token = match req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => return unauth("missing or malformed Authorization header"),
    };

    let claims = match state.tokens.get(&token).cloned() {
        Some(c) => c,
        None => return unauth("invalid bearer token"),
    };

    // Attach claims for downstream consumers (rate-limit + handlers). The
    // rate-limit `KeyExtractor` reads `AuthClaims.tenant` from the request
    // extensions to key per tenant (D-08).
    req.extensions_mut().insert(AuthClaims {
        tenant: claims.tenant.clone(),
        scopes: Arc::new(claims.scopes.clone()),
    });

    // Scope check — `RequiredScope` was attached by the per-route layer.
    if let Some(req_scope) = req.extensions().get::<RequiredScope>().cloned()
        && !claims.scopes.iter().any(|s| s == req_scope.0)
    {
        return forbidden(req_scope.0);
    }

    next.run(req).await
}

fn unauth(msg: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "unauthorized", "message": msg })))
        .into_response()
}

fn forbidden(scope: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": "forbidden",
            "message": format!("token lacks required scope `{scope}`"),
        })),
    )
        .into_response()
}
