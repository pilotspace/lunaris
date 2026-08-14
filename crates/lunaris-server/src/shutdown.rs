//! Plan 05-01 — graceful shutdown via `tokio::sync::Notify`, plus the 0.6.2
//! P0-1 **bounded** drain ([`serve_with_deadline`]).
//!
//! Mirrors `crates/lunaris-verify/src/worker.rs:88-100` shape. `main.rs`
//! `tokio::select!`s ctrl-c / SIGTERM and calls [`Shutdown::trigger`], which
//! cascades into the drain.
//!
//! ## Why the drain needs a deadline (0.6.2 P0-1)
//!
//! Until 0.6.2 the server ran a bare
//! `axum::serve(..).with_graceful_shutdown(shutdown.wait())`: `grace_secs` was
//! stored and never read, so a single wedged in-flight request (a stalled Moon
//! write, a hung upstream LLM call) kept the process alive indefinitely and the
//! orchestrator escalated to SIGKILL — killing the OTHER, healthy in-flight
//! requests too. [`serve_with_deadline`] bounds phase two of the shutdown at
//! `grace_secs` while preserving the fast path: an idle server returns in
//! milliseconds, it never sleeps out the window.
//!
//! ## Why `trigger()` latches
//!
//! `Notify::notify_waiters()` only wakes waiters that have ALREADY registered.
//! A SIGTERM that lands between `Shutdown::new` and the first poll of the serve
//! future was therefore dropped on the floor and the process hung forever.
//! [`Shutdown`] now carries an `AtomicBool` latch that [`Shutdown::wait`]
//! checks before *and* after registering its `Notified` (via
//! `Notified::enable`), which closes the race in both directions.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

#[derive(Clone, Debug)]
pub struct Shutdown {
    notify: Arc<Notify>,
    /// Latch so a `trigger()` that races registration is never lost.
    triggered: Arc<AtomicBool>,
    grace_secs: u64,
}

impl Shutdown {
    /// Construct a fresh shutdown handle with the given grace window.
    pub fn new(grace_secs: u64) -> Self {
        Self {
            notify: Arc::new(Notify::new()),
            triggered: Arc::new(AtomicBool::new(false)),
            grace_secs,
        }
    }

    /// Borrow the inner `Arc<Notify>` (e.g., for embedding in another future).
    ///
    /// NOTE: waiters built directly from this handle do NOT see the latch — a
    /// `trigger()` that fires before they register is lost. It is fine for the
    /// long-lived background pollers (spawned at boot, before any signal can
    /// arrive); anything on the shutdown critical path must use
    /// [`Shutdown::wait`].
    pub fn notify(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    /// Configured grace window in seconds.
    pub fn grace_secs(&self) -> u64 {
        self.grace_secs
    }

    /// `true` once [`Shutdown::trigger`] has been called.
    pub fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::Acquire)
    }

    /// Resolve when `trigger()` is called — including when it was called
    /// BEFORE this future was first polled.
    pub async fn wait(&self) {
        if self.is_triggered() {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        // Register with the Notify BEFORE the second latch read, so a
        // concurrent `trigger()` either sets the latch (we see it below) or
        // wakes this now-registered waiter. No interleaving loses the signal.
        notified.as_mut().enable();
        if self.is_triggered() {
            return;
        }
        notified.await;
    }

    /// Wake every waiter. Idempotent — extra calls are no-ops once already
    /// triggered.
    pub fn trigger(&self) {
        // Latch FIRST, then wake: a waiter that registers between the two
        // still observes the latch on its post-registration re-check.
        self.triggered.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

/// Serve `app` on `listener` until [`Shutdown::trigger`] fires, then drain
/// in-flight requests for **at most** `shutdown.grace_secs()`.
///
/// Two phases:
///
/// 1. **Serving.** Await the axum serve future and the shutdown signal
///    together. If the server ends on its own (accept-loop IO error) we return
///    that error unchanged.
/// 2. **Bounded drain.** Once signalled, axum stops accepting and keeps polling
///    in-flight requests; we wrap that in `tokio::time::timeout(grace)`. If the
///    drain finishes first we return its result (the fast path — an idle server
///    returns in milliseconds). If the window expires we log at WARN with the
///    number of requests being abandoned (`aborted_in_flight`, read from the
///    `lunaris_http_in_flight` gauge) and return `Ok(())` so the caller exits
///    promptly; dropping the tokio runtime cancels whatever is left.
///
/// `grace_secs == 0` means "abandon in-flight work immediately".
pub async fn serve_with_deadline(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    shutdown: Shutdown,
) -> std::io::Result<()> {
    use std::future::IntoFuture;

    let grace_secs = shutdown.grace_secs();
    let grace = Duration::from_secs(grace_secs);

    // The graceful-shutdown signal future is owned by axum, so we tee it
    // through a oneshot to learn *when* the drain started without registering
    // a second `Notify` waiter (which would reintroduce the lost-wakeup race).
    let (signalled_tx, signalled_rx) = tokio::sync::oneshot::channel::<()>();
    let drain = shutdown.clone();
    let serve = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            drain.wait().await;
            let _ = signalled_tx.send(());
        })
        .into_future();
    tokio::pin!(serve);

    // Phase 1 — serve until the signal (or an accept-loop error).
    tokio::select! {
        res = &mut serve => return res,
        _ = signalled_rx => {}
    }

    tracing::info!(grace_secs, "shutdown signalled; draining in-flight requests");

    // Phase 2 — bounded drain.
    match tokio::time::timeout(grace, &mut serve).await {
        Ok(res) => {
            tracing::info!("in-flight requests drained; exiting cleanly");
            res
        }
        Err(_) => {
            let aborted_in_flight = crate::metrics::metrics().http_in_flight.get();
            tracing::warn!(
                grace_secs,
                aborted_in_flight,
                "shutdown grace window expired; abandoning in-flight requests"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn shutdown_wait_returns_after_trigger() {
        let s = Shutdown::new(30);
        let s2 = s.clone();
        let task = tokio::spawn(async move {
            s2.wait().await;
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        s.trigger();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown wait must complete within 1s of trigger")
            .expect("task succeeded");
    }

    #[tokio::test]
    async fn shutdown_wait_returns_when_already_triggered() {
        let s = Shutdown::new(30);
        s.trigger();
        assert!(s.is_triggered());
        tokio::time::timeout(Duration::from_secs(1), s.wait())
            .await
            .expect("a pre-triggered shutdown must not park the waiter forever");
    }

    #[test]
    fn grace_secs_round_trips() {
        let s = Shutdown::new(45);
        assert_eq!(s.grace_secs(), 45);
    }
}
