//! RED suite for 0.6.2 P0-1 — the shutdown grace deadline.
//!
//! The gap (0.6.2 audit): `crates/lunaris-server/src/shutdown.rs` stores
//! `grace_secs` and exposes `grace_secs()`, but **nothing consumes it**.
//! `main.rs` does a bare
//! `axum::serve(..).with_graceful_shutdown(shutdown.wait())`, so the drain has
//! no deadline at all — one hung in-flight request pins the process until the
//! orchestrator escalates to SIGKILL. Every rolling deploy pays that cost.
//!
//! Contract driven here:
//!   1. after the shutdown signal fires, the serve future MUST return within
//!      `grace_secs` (+ scheduling slack), even with an in-flight request that
//!      never completes;
//!   2. an IDLE drain must still return promptly — the deadline is a ceiling,
//!      not a mandatory sleep (this is the regression guard against
//!      "just sleep(grace) then exit");
//!   3. a shutdown signal that fires BEFORE the serve future is first polled
//!      must still be observed. `Notify::notify_waiters()` only wakes waiters
//!      that have already registered, so a SIGTERM racing startup currently
//!      hangs forever.
//!
//! RED reason: `lunaris_server::shutdown::serve_with_deadline` does not exist
//! yet, so this file fails to compile (E0425). Once it lands, (1) and (3) are
//! the behavioural asserts that keep it honest.

use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use lunaris_server::Shutdown;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A handler that never completes — the "wedged in-flight request" the
/// orchestrator currently SIGKILLs us over.
async fn hang() -> &'static str {
    std::future::pending::<()>().await;
    "unreachable"
}

async fn bind_ephemeral() -> (TcpListener, std::net::SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral");
    let addr = listener.local_addr().expect("local_addr");
    (listener, addr)
}

fn hang_app() -> Router {
    Router::new().route("/hang", get(hang))
}

#[tokio::test]
async fn drain_is_bounded_by_grace_secs() {
    let (listener, addr) = bind_ephemeral().await;
    let shutdown = Shutdown::new(1);
    let trigger = shutdown.clone();

    let server =
        tokio::spawn(lunaris_server::shutdown::serve_with_deadline(listener, hang_app(), shutdown));

    // Park one request inside the never-completing handler.
    let client = tokio::spawn(async move {
        let mut sock = TcpStream::connect(addr).await.expect("connect");
        sock.write_all(b"GET /hang HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n")
            .await
            .expect("write request");
        let mut buf = Vec::new();
        let _ = sock.read_to_end(&mut buf).await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    trigger.trigger();

    let outcome = tokio::time::timeout(Duration::from_secs(8), server).await;
    let elapsed = started.elapsed();
    client.abort();

    assert!(
        outcome.is_ok(),
        "serve future MUST return after the grace window; still draining {elapsed:?} after the \
         shutdown signal — an unbounded drain means the orchestrator SIGKILLs us"
    );
    outcome.expect("within outer bound").expect("serve task must not panic").expect("serve io");
    assert!(
        elapsed < Duration::from_secs(4),
        "drain must end within grace (1s) plus slack; took {elapsed:?}"
    );
}

#[tokio::test]
async fn idle_drain_returns_promptly_without_burning_the_grace_window() {
    let (listener, _addr) = bind_ephemeral().await;
    // 30s grace — a correct implementation returns in milliseconds because
    // there is nothing in flight.
    let shutdown = Shutdown::new(30);
    let trigger = shutdown.clone();

    let server =
        tokio::spawn(lunaris_server::shutdown::serve_with_deadline(listener, hang_app(), shutdown));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let started = Instant::now();
    trigger.trigger();
    let outcome = tokio::time::timeout(Duration::from_secs(5), server).await;
    let elapsed = started.elapsed();

    assert!(
        outcome.is_ok(),
        "an idle server must drain immediately, not sleep out the 30s grace window (took {elapsed:?})"
    );
    outcome.expect("within outer bound").expect("serve task must not panic").expect("serve io");
}

#[tokio::test]
async fn shutdown_fired_before_serve_is_still_observed() {
    let (listener, _addr) = bind_ephemeral().await;
    let shutdown = Shutdown::new(1);
    // SIGTERM racing startup: the signal fires before the serve future has
    // ever been polled, so no `Notify` waiter is registered yet.
    shutdown.trigger();

    let server =
        tokio::spawn(lunaris_server::shutdown::serve_with_deadline(listener, hang_app(), shutdown));

    let outcome = tokio::time::timeout(Duration::from_secs(5), server).await;
    assert!(
        outcome.is_ok(),
        "a shutdown triggered before the first poll MUST still be observed; \
         notify_waiters() only wakes ALREADY-registered waiters, so the drain hangs forever"
    );
    outcome.expect("within outer bound").expect("serve task must not panic").expect("serve io");
}
