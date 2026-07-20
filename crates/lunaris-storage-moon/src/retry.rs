//! Belt-and-suspenders connection-retry guard for the Moon read paths.
//!
//! The durable fix for "a dropped socket wedges every later command with
//! `broken pipe` forever" lives in the SDK: `moondb::MoonClient` now wraps a
//! `redis::aio::ConnectionManager` (see `vendor/moon/sdk/rust/src/client.rs`
//! + its `tests/reconnect.rs`), which transparently reconnects with backoff.
//!
//! This module is the SECOND layer: even with a reconnecting manager, the
//! single command that TRIGGERS a reconnect can still surface a transient
//! connection error before the heal completes. A single lunaris-layer retry
//! converts that one-shot blip into a success for the recall caller — so a
//! backend flip never bubbles a `broken pipe` up through `memory.recall`
//! (the exact production symptom this guards). It is deliberately narrow:
//! ONE retry, only on classified-transient connection faults, only on the
//! read paths (a write retry could double-apply a non-idempotent op).

use lunaris_core::error::StorageError;

/// Classify a [`StorageError`] as a transient connection fault worth exactly
/// one retry. Only `Backend` strings that name a socket-level failure qualify;
/// a `NotSupported`, a validation error, or a genuine server error (e.g. a
/// missing index) must NOT be retried — retrying those just doubles the
/// latency for the same failure.
pub(crate) fn is_transient_conn_error(e: &StorageError) -> bool {
    match e {
        StorageError::Backend(msg) => {
            let m = msg.to_ascii_lowercase();
            m.contains("broken pipe")
                || m.contains("connection reset")
                || m.contains("connection refused")
                || m.contains("connection aborted")
                || m.contains("not connected")
                || m.contains("connection closed")
        }
        _ => false,
    }
}

/// Run `op`, retrying it AT MOST ONCE if the first attempt fails with a
/// transient connection error (see [`is_transient_conn_error`]). Any other
/// error — and success — returns immediately without a second attempt.
///
/// `op` is an `FnMut` returning a fresh future each call so the retry dials a
/// new sub-client off the (by-then reconnected) `ConnectionManager`.
pub(crate) async fn with_conn_retry<T, F, Fut>(mut op: F) -> Result<T, StorageError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, StorageError>>,
{
    match op().await {
        Ok(v) => Ok(v),
        Err(e) if is_transient_conn_error(&e) => {
            tracing::warn!(
                error = %e,
                "moon: transient connection fault on a read — retrying once \
                 (ConnectionManager reconnects underneath)"
            );
            op().await
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn classifies_socket_faults_as_transient() {
        assert!(is_transient_conn_error(&StorageError::Backend(
            "moon: redis error: broken pipe".into()
        )));
        assert!(is_transient_conn_error(&StorageError::Backend(
            "Connection refused (os error 61)".into()
        )));
        assert!(is_transient_conn_error(&StorageError::Backend("connection reset by peer".into())));
    }

    #[test]
    fn does_not_retry_non_transient_errors() {
        // A real server error (missing index) or an unsupported command must
        // NOT be classified transient — retrying wastes a round-trip.
        assert!(!is_transient_conn_error(&StorageError::Backend(
            "moon: index not found: chunks".into()
        )));
        assert!(!is_transient_conn_error(&StorageError::NotSupported(
            "moon: command not supported on this server build"
        )));
    }

    #[tokio::test]
    async fn retries_once_then_succeeds_on_transient() {
        let calls = Cell::new(0);
        let out: Result<u8, StorageError> = with_conn_retry(|| {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move {
                if n == 1 {
                    Err(StorageError::Backend("moon: redis error: broken pipe".into()))
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(out.expect("second attempt succeeds"), 42);
        assert_eq!(calls.get(), 2, "must retry exactly once after a transient fault");
    }

    #[tokio::test]
    async fn does_not_retry_when_first_error_is_non_transient() {
        let calls = Cell::new(0);
        let out: Result<u8, StorageError> = with_conn_retry(|| {
            calls.set(calls.get() + 1);
            async move { Err(StorageError::Backend("moon: index not found".into())) }
        })
        .await;
        assert!(out.is_err(), "non-transient error propagates");
        assert_eq!(calls.get(), 1, "must NOT retry a non-transient error");
    }

    #[tokio::test]
    async fn stops_after_one_retry_even_if_still_transient() {
        // A persistently-down backend must not spin — the guard caps at 2 attempts.
        let calls = Cell::new(0);
        let out: Result<u8, StorageError> = with_conn_retry(|| {
            calls.set(calls.get() + 1);
            async move { Err(StorageError::Backend("broken pipe".into())) }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls.get(), 2, "at most one retry — never an unbounded loop");
    }

    #[tokio::test]
    async fn no_retry_on_immediate_success() {
        let calls = Cell::new(0);
        let out: Result<u8, StorageError> = with_conn_retry(|| {
            calls.set(calls.get() + 1);
            async move { Ok(7) }
        })
        .await;
        assert_eq!(out.expect("ok"), 7);
        assert_eq!(calls.get(), 1, "success path calls op exactly once");
    }
}
