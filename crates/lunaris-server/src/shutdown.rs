//! Plan 05-01 — graceful shutdown via `tokio::sync::Notify`.
//!
//! Mirrors `crates/lunaris-verify/src/worker.rs:88-100` shape verbatim. The
//! `axum::serve(...).with_graceful_shutdown(...)` future awaits `Shutdown::wait`
//! and `tokio::select!` against ctrl-c / SIGTERM in `main.rs` triggers the
//! Notify which cascades into the drain.

use std::sync::Arc;

use tokio::sync::Notify;

#[derive(Clone, Debug)]
pub struct Shutdown {
    notify: Arc<Notify>,
    grace_secs: u64,
}

impl Shutdown {
    /// Construct a fresh shutdown handle with the given grace window.
    pub fn new(grace_secs: u64) -> Self {
        Self { notify: Arc::new(Notify::new()), grace_secs }
    }

    /// Borrow the inner `Arc<Notify>` (e.g., for embedding in another future).
    pub fn notify(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    /// Configured grace window in seconds.
    pub fn grace_secs(&self) -> u64 {
        self.grace_secs
    }

    /// Resolve when `trigger()` is called.
    pub async fn wait(&self) {
        self.notify.notified().await;
    }

    /// Wake every waiter. Idempotent — extra calls are no-ops once already
    /// triggered.
    pub fn trigger(&self) {
        self.notify.notify_waiters();
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

    #[test]
    fn grace_secs_round_trips() {
        let s = Shutdown::new(45);
        assert_eq!(s.grace_secs(), 45);
    }
}
