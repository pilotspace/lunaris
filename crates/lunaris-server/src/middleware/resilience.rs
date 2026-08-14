//! 0.6.2 P0-1/P0-2 — HTTP resilience layers.
//!
//! Three concerns, in the order a request meets them:
//!
//! 1. [`load_shed_middleware`] — concurrency limit + load shedding (P0-2);
//! 2. [`in_flight_middleware`] — the `lunaris_http_in_flight` gauge the bounded
//!    shutdown drain reports on (P0-1);
//! 3. [`timeout_middleware`] — the per-request wall-clock budget (P0-2).
//!
//! ## Why shed OUTSIDE the limit, and timeout INSIDE it
//!
//! The canonical tower spelling of this is
//! `LoadShedLayer` wrapping `GlobalConcurrencyLimitLayer` wrapping the app:
//! the limit reports "not ready" when it is saturated, and load-shed converts
//! that not-readiness into an immediate error instead of parking the caller.
//! The order is not cosmetic — with the two swapped, or with the limit alone,
//! an over-cap request *waits* for a permit, which is precisely the unbounded
//! queue we are trying to remove (a client that has already given up still
//! holds a slot, and the queue is the thing that OOMs us).
//!
//! We spell it as ONE middleware built on `Semaphore::try_acquire_owned`
//! instead of two tower layers because the try-acquire IS the fused
//! limit-then-shed decision, and doing it by hand gives us the
//! `lunaris_http_shed_total` counter at the exact rejection point (tower's
//! `LoadShed` surfaces a `BoxError` we would have to re-classify) without
//! pulling `HandleErrorLayer` into the stack to get back to `Infallible`.
//!
//! The timeout goes INSIDE the limit so a permit is held for at most the
//! request budget: the budget then covers real handler work only, and the
//! saturation ceiling has a bounded drain time (`concurrency × timeout` is the
//! worst-case backlog, not infinity). Outside the limit it would be spent
//! waiting for a permit — a request could burn its whole budget and expire
//! having never run.
//!
//! In-flight accounting sits between them so the gauge counts requests that
//! are actually being served (a shed request never becomes in-flight).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::sync::Semaphore;

use crate::metrics::metrics;

/// `Retry-After` delta-seconds advertised on a shed response. One second: the
/// cap is a *concurrency* ceiling, not a rate limit, so capacity usually
/// returns within a request's service time.
const SHED_RETRY_AFTER_SECS: u64 = 1;

/// Boxed middleware future — the shape `axum::middleware::from_fn` wants from
/// a closure that captures configuration (same idiom as `lib.rs::scoped_auth`).
type MiddlewareFuture = Pin<Box<dyn Future<Output = Response> + Send + 'static>>;

/// RAII in-flight accounting. Decrements on `Drop`, NOT on the happy path
/// only — an axum request future is dropped when the client disconnects or a
/// timeout abandons it, and a hand-rolled `inc()/dec()` pair would leak the
/// gauge upward forever in exactly the situations we most need it to be right.
struct InFlightGuard;

impl InFlightGuard {
    fn enter() -> Self {
        metrics().http_in_flight.inc();
        Self
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        metrics().http_in_flight.dec();
    }
}

/// Maintain `lunaris_http_in_flight` for the duration of every request.
pub async fn in_flight_middleware(req: Request, next: Next) -> Response {
    let _guard = InFlightGuard::enter();
    next.run(req).await
}

/// Per-request wall-clock budget. Over-budget requests are cut off with
/// `408 Request Timeout` and counted in `lunaris_http_timeout_total`.
///
/// `secs == 0` disables the budget — the returned middleware then just
/// forwards, so operators who bound requests at their proxy pay nothing.
///
/// The budget covers producing the response, NOT streaming its body: a
/// `/v1/recall` SSE stream returns its `Sse` response immediately and keeps
/// streaming afterwards.
pub fn timeout_middleware(
    secs: u64,
) -> impl Clone + Send + Sync + 'static + Fn(Request, Next) -> MiddlewareFuture {
    let budget = (secs > 0).then(|| Duration::from_secs(secs));
    move |req, next| {
        Box::pin(async move {
            let Some(budget) = budget else {
                return next.run(req).await;
            };
            let method = req.method().clone();
            let path = req.uri().path().to_string();
            match tokio::time::timeout(budget, next.run(req)).await {
                Ok(resp) => resp,
                Err(_) => {
                    metrics().http_timeout_total.inc();
                    tracing::warn!(
                        %method,
                        %path,
                        timeout_secs = secs,
                        "request exceeded the configured budget; answering 408"
                    );
                    (
                        StatusCode::REQUEST_TIMEOUT,
                        Json(serde_json::json!({
                            "error": "request_timeout",
                            "message": format!("request exceeded the {secs}s server budget"),
                        })),
                    )
                        .into_response()
                }
            }
        })
    }
}

/// Concurrency limit fused with load shedding: acquire a permit or reject.
///
/// A caller that cannot get a permit immediately gets `503 Service Unavailable`
/// with `Retry-After`, and is counted in `lunaris_http_shed_total`. It is NEVER
/// queued — see the module docs for why waiting is the failure mode we are
/// removing.
///
/// `limit == 0` disables the cap.
pub fn load_shed_middleware(
    limit: usize,
) -> impl Clone + Send + Sync + 'static + Fn(Request, Next) -> MiddlewareFuture {
    let permits = (limit > 0).then(|| Arc::new(Semaphore::new(limit)));
    move |req, next| {
        let permits = permits.clone();
        Box::pin(async move {
            let Some(permits) = permits else {
                return next.run(req).await;
            };
            match permits.try_acquire_owned() {
                Ok(permit) => {
                    let resp = next.run(req).await;
                    // Explicit: the permit is released HERE, when the request
                    // is done — including when the future is dropped, since
                    // OwnedSemaphorePermit releases on Drop.
                    drop(permit);
                    resp
                }
                Err(_) => {
                    metrics().http_shed_total.inc();
                    tracing::warn!(
                        method = %req.method(),
                        path = req.uri().path(),
                        limit,
                        "concurrency limit reached; shedding request with 503"
                    );
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [(header::RETRY_AFTER, SHED_RETRY_AFTER_SECS.to_string())],
                        Json(serde_json::json!({
                            "error": "overloaded",
                            "message": format!(
                                "server is at its concurrency limit of {limit} in-flight requests"
                            ),
                        })),
                    )
                        .into_response()
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_guard_is_balanced_and_drop_safe() {
        let before = metrics().http_in_flight.get();
        {
            let _g = InFlightGuard::enter();
            assert_eq!(metrics().http_in_flight.get(), before + 1);
        }
        assert_eq!(metrics().http_in_flight.get(), before, "guard must decrement on drop");
    }
}
