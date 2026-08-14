//! 0.6.2 P0-1/P0-2 — HTTP resilience layers.
//!
//! Today this module owns the in-flight accounting that the bounded shutdown
//! drain (`shutdown::serve_with_deadline`) reports on. P0-2 extends it with the
//! request timeout and the concurrency limit / load-shed pair.

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

use crate::metrics::metrics;

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
