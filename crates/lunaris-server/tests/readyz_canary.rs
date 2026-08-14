//! RED suite for 0.6.2 P0-3 — split liveness from readiness, with a WRITE
//! canary.
//!
//! The gap (0.6.2 audit): `/healthz` performs a bare Moon PING and there is no
//! `/readyz` at all. A Moon write-stall wedge — the exact incident class this
//! repo has hit twice (MA1 write-stall, dashtable recovery crash-loop) —
//! survives PING: the socket accepts, the PING returns, and the load balancer
//! keeps routing traffic into a store that cannot accept a single write. The
//! outage is invisible to every probe we ship.
//!
//! Design ruling (0.6.2 context, already decided):
//!   `/healthz` = cheap LIVENESS (process up), Kubernetes livenessProbe.
//!   `/readyz`  = READINESS: PING + a write canary (SET/DEL of a reserved key
//!                in a reserved internal scope, 2 s timeout) + embedder state,
//!                Kubernetes readinessProbe. Exports a `lunaris_ready` gauge.
//!                The canary is rate-limited — a result ≤ 5 s old is served
//!                from cache so probe traffic cannot become write traffic.
//!
//! The load-bearing test is `stalled_writes_turn_readyz_red_while_healthz_stays_green`:
//! a backend that ACCEPTS and PINGs but STALLS writes must fail readiness while
//! liveness stays green (fail readiness → drop out of the LB; fail liveness →
//! get restarted, which does not fix a wedged backend).
//!
//! RED reason: there is no `/readyz` route, so the router answers 404.

mod support;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use support::{StubStorage, build_app, test_config};
use tower::ServiceExt;

async fn get(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder().uri(uri).body(Body::empty()).expect("request");
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 256 * 1024).await.expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn get_text(app: axum::Router, uri: &str) -> (StatusCode, String) {
    let req = Request::builder().uri(uri).body(Body::empty()).expect("request");
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn readyz_is_green_when_ping_and_write_canary_succeed() {
    let app = build_app(test_config("ready-ok"), StubStorage::healthy());
    let (status, body) = get(app, "/readyz").await;
    assert_eq!(status, StatusCode::OK, "a healthy backend must answer 200 on /readyz");
    assert_eq!(body["ready"], serde_json::Value::Bool(true));
    assert_eq!(body["checks"]["ping"], "ok");
    assert_eq!(body["checks"]["canary"], "ok");
    assert_eq!(body["checks"]["embedder"], "ok");
}

#[tokio::test]
async fn stalled_writes_turn_readyz_red_while_healthz_stays_green() {
    // The wedge: PING answers instantly, writes never return.
    let storage = StubStorage::healthy().with_write_delay(Duration::from_secs(60));
    let app = build_app(test_config("ready-wedge"), storage);

    let (live_status, live_body) = get(app.clone(), "/healthz").await;
    assert_eq!(
        live_status,
        StatusCode::OK,
        "liveness must stay GREEN under a write wedge — restarting the process does not \
         un-wedge the backend"
    );
    assert_eq!(live_body["ok"], serde_json::Value::Bool(true));

    let started = Instant::now();
    let (ready_status, ready_body) = get(app, "/readyz").await;
    let elapsed = started.elapsed();

    assert_eq!(
        ready_status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a backend that PINGs but cannot WRITE must fail readiness — this is the entire \
         point of the canary"
    );
    assert_eq!(ready_body["ready"], serde_json::Value::Bool(false));
    assert_eq!(ready_body["checks"]["ping"], "ok", "PING itself is fine — that is the trap");
    assert_eq!(ready_body["checks"]["canary"], "timeout");
    assert!(
        elapsed < Duration::from_secs(5),
        "the canary must be bounded by its own 2s timeout, not by the request budget; \
         took {elapsed:?}"
    );
}

#[tokio::test]
async fn failed_ping_turns_readyz_red() {
    let app =
        build_app(test_config("ready-ping-fail"), StubStorage::healthy().with_health_failure());
    let (status, body) = get(app, "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["ready"], serde_json::Value::Bool(false));
    assert_eq!(body["checks"]["ping"], "error");
}

#[tokio::test]
async fn rejected_writes_turn_readyz_red() {
    let app =
        build_app(test_config("ready-write-fail"), StubStorage::healthy().with_write_failure());
    let (status, body) = get(app, "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["checks"]["ping"], "ok");
    assert_eq!(body["checks"]["canary"], "error");
}

#[tokio::test]
async fn healthz_stays_cheap_and_never_writes() {
    let storage = StubStorage::healthy();
    let writes = storage.write_counter();
    let app = build_app(test_config("health-cheap"), storage);

    let (status, _) = get(app, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "liveness must stay cheap — the write canary belongs to /readyz only"
    );
}

#[tokio::test]
async fn the_canary_is_rate_limited_so_probes_do_not_become_write_traffic() {
    let storage = StubStorage::healthy();
    let writes = storage.write_counter();
    let app = build_app(test_config("ready-cached"), storage);

    let (first_status, _) = get(app.clone(), "/readyz").await;
    assert_eq!(first_status, StatusCode::OK);
    let after_first = writes.load(Ordering::SeqCst);
    assert!(after_first > 0, "the first probe must actually exercise the write path");

    // Kubernetes default readinessProbe cadence is ~10s, but nothing stops an
    // operator (or a scrape loop, or a flapping LB) from hammering it.
    for _ in 0..20 {
        let (status, body) = get(app.clone(), "/readyz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ready"], serde_json::Value::Bool(true));
    }
    assert_eq!(
        writes.load(Ordering::SeqCst),
        after_first,
        "a result ≤5s old must be served from cache — otherwise probe traffic IS write \
         traffic and a busy LB DDoSes the store"
    );
}

#[tokio::test]
async fn readyz_exports_the_lunaris_ready_gauge() {
    let app = build_app(test_config("ready-gauge"), StubStorage::healthy());
    let (status, _) = get(app.clone(), "/readyz").await;
    assert_eq!(status, StatusCode::OK);

    let (metrics_status, text) = get_text(app, "/metrics").await;
    assert_eq!(metrics_status, StatusCode::OK);
    assert!(
        text.contains("lunaris_ready"),
        "/metrics must export the lunaris_ready gauge so readiness is alertable, got:\n{text}"
    );
}
