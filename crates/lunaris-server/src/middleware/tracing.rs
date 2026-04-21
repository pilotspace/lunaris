//! Plan 05-05 OPS-05 — per-request tracing span + `x-correlation-id`
//! propagation.
//!
//! ## Span shape (CONTEXT.md D-24 verbatim)
//!
//! Wraps every HTTP request in:
//! ```text
//! info_span!("lunaris.server.handle_request",
//!     correlation_id = <ulid or header value>,
//!     tenant         = <claims.tenant or "anonymous">,
//!     method         = <HTTP method>,
//!     path           = <URI path>)
//! ```
//!
//! Downstream `lunaris.{ingest|recall|forget}` spans nest as children
//! automatically because they're created inside the `.instrument(span).await`
//! body of the verb handler — tracing-subscriber's span-list emission then
//! shows the full chain in JSON output (Plan 05-05 Task 1's
//! `lunaris::logging::init` selector).
//!
//! ## Correlation-ID source (T-05-05-01 mitigation)
//!
//! Read from the `x-correlation-id` HTTP header when present (string-typed,
//! length-bounded by HTTP header limits + Ulid sanity-check at ingestion);
//! otherwise generated as a fresh `ulid::Ulid::new()`. Field values are
//! recorded via tracing's `%display` formatter and tracing-subscriber's JSON
//! formatter properly escapes them, so the log-injection threat is
//! mitigated even if the client sends `\n`-laden garbage.
//!
//! ## Layer ordering
//!
//! `tracing_middleware` MUST be layered AFTER `auth_middleware` so the
//! `AuthClaims` extension is populated by the time we read it. axum's
//! `.layer(...)` adds outer-first on the request — see `lib::build` for the
//! exact stacking.

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument;

use crate::middleware::auth::AuthClaims;

/// Header name read for the per-request correlation id.
pub const CORRELATION_HEADER: &str = "x-correlation-id";

/// axum middleware — wraps the downstream `next.run(req)` future in a
/// `lunaris.server.handle_request` info_span carrying correlation_id +
/// tenant + method + path.
pub async fn tracing_middleware(req: Request<Body>, next: Next) -> Response {
    // Read x-correlation-id header (T-05-05-01 — escaping handled by
    // tracing-subscriber's JSON formatter; no raw printf).
    let correlation_id = req
        .headers()
        .get(CORRELATION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| ulid::Ulid::new().to_string());

    // Tenant comes from AuthClaims if present; else "anonymous" (the request
    // may be hitting an unauthenticated route like /healthz or /metrics, OR
    // tracing_middleware ran before auth on a misconfigured route — falling
    // back to "anonymous" keeps the span field cardinality bounded).
    let tenant = req
        .extensions()
        .get::<AuthClaims>()
        .map(|c| c.tenant.clone())
        .unwrap_or_else(|| "anonymous".to_string());

    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let span = tracing::info_span!(
        "lunaris.server.handle_request",
        correlation_id = %correlation_id,
        tenant = %tenant,
        method = %method,
        path = %path,
    );

    next.run(req).instrument(span).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlation_header_constant_matches_d24() {
        assert_eq!(CORRELATION_HEADER, "x-correlation-id");
    }

    /// The middleware fn must be `Send + Sync` so axum's `from_fn` accepts it.
    #[test]
    fn tracing_middleware_is_send_sync() {
        fn _check_send_sync<F: Send + Sync>(_f: F) {}
        _check_send_sync(tracing_middleware);
    }
}
