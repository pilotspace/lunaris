//! RED suite for 0.6.2 P0-2 — request timeout + concurrency limit + load shed.
//!
//! The gap (0.6.2 audit): `lunaris-server`'s tower stack is `cors + trace` and
//! nothing else. There is no request timeout, no concurrency limit and no load
//! shedding anywhere in the crate — a slow backend converts directly into
//! unbounded in-flight requests, unbounded memory, and a queue that only ever
//! drains into client timeouts.
//!
//! Design ruling (0.6.2 context, already decided): 30 s request timeout, 256
//! concurrent requests, shed with `503 + Retry-After`; all three overridable
//! via `--http-timeout-secs` / `LUNARIS_HTTP_TIMEOUT_SECS` and
//! `--http-concurrency` / `LUNARIS_HTTP_CONCURRENCY`.
//!
//! Every assertion here drives the **production** router (`lunaris_server::build`)
//! over a `StubStorage`, not a synthetic router with the layers hand-applied —
//! "built ≠ wired". `/healthz` is the vehicle because it needs no auth and its
//! latency is exactly `StubStorage::health_delay`.
//!
//! RED reason: `Config` has no `http_timeout_secs` / `http_concurrency` field
//! yet, so `support::test_config` fails to compile (E0560); once the fields
//! land, the shed/timeout assertions still fail until the layers are wired.

mod support;

use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use support::{StubStorage, build_app, test_config};
use tower::ServiceExt;

fn healthz_request() -> Request<Body> {
    Request::builder().uri("/healthz").body(Body::empty()).expect("request")
}

#[tokio::test]
async fn slow_handler_is_cut_off_at_the_configured_timeout() {
    let mut cfg = test_config("timeout");
    cfg.http_timeout_secs = 1;
    // Backend parks for 30 s — 30x the configured request budget.
    let app = build_app(cfg, StubStorage::healthy().with_health_delay(Duration::from_secs(30)));

    let started = Instant::now();
    let resp = tokio::time::timeout(Duration::from_secs(10), app.oneshot(healthz_request()))
        .await
        .expect("the request MUST be cut off by the server, not by this test's outer bound")
        .expect("oneshot");
    let elapsed = started.elapsed();

    assert_eq!(
        resp.status(),
        StatusCode::REQUEST_TIMEOUT,
        "a request exceeding --http-timeout-secs must answer 408, got {}",
        resp.status()
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "the 1s timeout must fire promptly; took {elapsed:?}"
    );
    let body = to_bytes(resp.into_body(), 64 * 1024).await.expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json envelope");
    assert_eq!(json["error"], "request_timeout");
}

#[tokio::test]
async fn fast_requests_are_untouched_by_the_timeout() {
    let cfg = test_config("timeout-fast");
    let app = build_app(cfg, StubStorage::healthy());
    let resp = app.oneshot(healthz_request()).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK, "a fast request must not be timed out");
}

#[tokio::test]
async fn requests_beyond_the_concurrency_cap_are_shed_immediately() {
    let mut cfg = test_config("shed");
    cfg.http_concurrency = 1;
    cfg.http_timeout_secs = 30;
    let app = build_app(cfg, StubStorage::healthy().with_health_delay(Duration::from_secs(3)));

    // Occupy the single permit.
    let occupied = tokio::spawn(app.clone().oneshot(healthz_request()));
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The second request must be REJECTED, not queued behind a 3s handler.
    let started = Instant::now();
    let resp = app.oneshot(healthz_request()).await.expect("oneshot");
    let elapsed = started.elapsed();

    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a request beyond --http-concurrency must be shed with 503, got {}",
        resp.status()
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "load shedding must be IMMEDIATE (fail fast), not queue-then-serve; took {elapsed:?}"
    );
    let retry_after = resp
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .expect("a shed response must carry Retry-After so clients back off")
        .to_str()
        .expect("ascii")
        .to_string();
    assert!(retry_after.parse::<u64>().is_ok(), "Retry-After must be delta-seconds, got {retry_after:?}");
    let body = to_bytes(resp.into_body(), 64 * 1024).await.expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json envelope");
    assert_eq!(json["error"], "overloaded");

    occupied.abort();
}

#[tokio::test]
async fn requests_within_the_concurrency_cap_are_served() {
    let mut cfg = test_config("shed-under");
    cfg.http_concurrency = 4;
    let app = build_app(cfg, StubStorage::healthy());

    for _ in 0..8 {
        let resp = app.clone().oneshot(healthz_request()).await.expect("oneshot");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "sequential requests must never be shed — the limit is CONCURRENCY, not a rate"
        );
    }
}

#[tokio::test]
async fn a_shed_response_releases_the_permit_for_the_next_caller() {
    let mut cfg = test_config("shed-release");
    cfg.http_concurrency = 1;
    let app = build_app(cfg, StubStorage::healthy().with_health_delay(Duration::from_millis(400)));

    let occupied = tokio::spawn(app.clone().oneshot(healthz_request()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let shed = app.clone().oneshot(healthz_request()).await.expect("oneshot");
    assert_eq!(shed.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Once the in-flight request finishes, the permit must be back.
    let _ = occupied.await.expect("join");
    let after = app.oneshot(healthz_request()).await.expect("oneshot");
    assert_eq!(
        after.status(),
        StatusCode::OK,
        "the concurrency permit must be released when the request completes"
    );
}

#[tokio::test]
async fn zero_disables_both_knobs() {
    let mut cfg = test_config("disabled");
    cfg.http_timeout_secs = 0;
    cfg.http_concurrency = 0;
    let app = build_app(cfg, StubStorage::healthy().with_health_delay(Duration::from_millis(200)));

    let resp = app.oneshot(healthz_request()).await.expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "0 means 'disabled' for both knobs — an escape hatch for operators who front the \
         server with their own limiter"
    );
}
