//! Plan 05-05 OPS-06 — `GET /metrics` Prometheus text-format handler.
//!
//! ## Behaviour
//!
//! - Default: `200 OK` with `Content-Type: text/plain; version=0.0.4; charset=utf-8`
//!   (the `prometheus::TextEncoder::format_type()` value) and the encoded
//!   body produced by `prometheus::gather() + TextEncoder::encode`.
//! - When `--metrics-disabled` (config flag) flips `runtime_flags.metrics_disabled`
//!   to `true`: `404 Not Found` so Prometheus scrapers see a clean disable
//!   rather than an empty body that looks like a stale exporter.
//!
//! ## No auth (matches `/healthz` shape)
//!
//! Mounted at the root router (NOT under `/v1`) so Prometheus scrapers don't
//! need to carry a Bearer token. Operators MUST front this with network-level
//! ACL or reverse-proxy auth in production (T-05-05-05 accept disposition;
//! standard Prometheus convention — documented in spec markdown).

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use prometheus::{Encoder, TextEncoder};

use crate::state::AppState;

pub async fn metrics_handler(State(state): State<AppState>) -> Response {
    if *state.runtime_flags.metrics_disabled.read() {
        return (StatusCode::NOT_FOUND, "metrics disabled at startup via --metrics-disabled\n")
            .into_response();
    }

    // Force the lazy-static registry to initialize so a fresh process serves
    // the 9-metric catalogue at zero values (rather than an empty body until
    // the first verb call lands).
    let _ = crate::metrics::metrics();

    let mut buf = Vec::with_capacity(8192);
    let encoder = TextEncoder::new();
    if let Err(e) = encoder.encode(&prometheus::gather(), &mut buf) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("metrics encode failed: {e}"))
            .into_response();
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, encoder.format_type())
        .body(Body::from(buf))
        .expect("build /metrics response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn metrics_singleton_is_idempotent() {
        // The metrics() registry init returns the same singleton (idempotency
        // contract — protects against double-registration panics).
        let m1 = crate::metrics::metrics();
        let m2 = crate::metrics::metrics();
        assert!(std::ptr::eq(m1, m2), "metrics() must return the same OnceLock singleton");
    }

    #[tokio::test]
    async fn text_encoder_format_type_is_prometheus_v004() {
        // Sanity check on the prometheus crate's content-type string — drift
        // here would mean Prometheus scrapers parse the body as opaque text.
        let encoder = TextEncoder::new();
        let ct = encoder.format_type();
        assert!(ct.contains("text/plain"), "format_type must include text/plain; got `{ct}`");
    }
}
