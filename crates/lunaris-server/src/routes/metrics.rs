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

    let mut buf = Vec::with_capacity(8192);
    let encoder = TextEncoder::new();
    if let Err(e) = encode_scrape(&encoder, &mut buf) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("metrics encode failed: {e}"))
            .into_response();
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, encoder.format_type())
        .body(Body::from(buf))
        .expect("build /metrics response")
}

/// Everything a scrape does except the disabled-flag check and the HTTP frame.
///
/// Split out of [`metrics_handler`] so a test can drive the SAME steps in the
/// same order without building an `AppState`. A test that called
/// `sync_audit_drops` itself would prove the sync works and say nothing about
/// whether a scrape reaches it — which is the failure mode that matters here,
/// since a series that is never synced looks identical to one that is always
/// zero.
fn encode_scrape(encoder: &TextEncoder, buf: &mut Vec<u8>) -> Result<(), prometheus::Error> {
    // Force the lazy-static registry to initialize so a fresh process serves
    // the metric catalogue at zero values (rather than an empty body until
    // the first verb call lands).
    let _ = crate::metrics::metrics();

    // W4.6 / D6.3 — the drop counter lives in `lunaris-core` as a plain atomic
    // (core has no metrics dependency, and audit drops happen on every surface,
    // not just this one). Mirror it into the registry before encoding, or the
    // exported series is frozen at whatever it held when the process started.
    crate::metrics::sync_audit_drops();

    encoder.encode(&prometheus::gather(), buf)
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

    /// W4.6 / D6.3 — a SCRAPE must pick up a drop that happened after the
    /// process started.
    ///
    /// `metrics.rs` already proves `sync_audit_drops` moves the series. This
    /// proves the scrape path calls it: without the sync, the exported value
    /// is whatever it held at registration and a real audit gap reads as zero
    /// forever.
    #[tokio::test]
    async fn a_scrape_picks_up_a_drop_that_happened_after_startup() {
        use lunaris_core::Scope;
        use lunaris_core::audit::{
            AuditEvent, ForgetReceiptData, ForgetTargetData, IndexKindData, PublishError,
            Publisher, ScopeSpecData, publish_audit_event,
        };
        use lunaris_core::storage::types::Lsn;

        struct FailingPublisher;
        #[async_trait::async_trait]
        impl Publisher for FailingPublisher {
            async fn publish(
                &self,
                _scope: &Scope,
                _topic: &str,
                _partition: u16,
                _payload: bytes::Bytes,
            ) -> Result<u64, PublishError> {
                Err(PublishError::Backend("broker down".into()))
            }
        }

        fn dropped_in(body: &str) -> u64 {
            body.lines()
                .find(|l| l.starts_with("lunaris_audit_events_dropped_total "))
                .and_then(|l| l.rsplit(' ').next())
                .and_then(|v| v.parse::<f64>().ok())
                .map(|v| v as u64)
                .expect("scrape body must carry lunaris_audit_events_dropped_total")
        }

        let encoder = TextEncoder::new();
        let mut before = Vec::new();
        encode_scrape(&encoder, &mut before).expect("encode");
        let before = dropped_in(&String::from_utf8(before).expect("utf8"));

        publish_audit_event(
            &FailingPublisher,
            &Scope::new("tenant-a").unwrap(),
            AuditEvent::Forget(ForgetReceiptData {
                target: ForgetTargetData::Scope(ScopeSpecData::BySource("x".into())),
                indices_affected: vec![IndexKindData::Kv],
                rows_written: 1,
                rows_deleted: 0,
                audit_lsn: Lsn { wall_ms: 1, counter: 0 },
                preview: false,
            }),
        )
        .await
        .expect("fire-and-forget must not propagate");

        let mut after = Vec::new();
        encode_scrape(&encoder, &mut after).expect("encode");
        let after = dropped_in(&String::from_utf8(after).expect("utf8"));

        assert!(
            after > before,
            "a scrape taken after a dropped audit event still reports {before}; the scrape path \
             does not sync the core counter, so an audit gap is invisible to Prometheus"
        );
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
