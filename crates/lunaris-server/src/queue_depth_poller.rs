//! Plan 05-05 OPS-06 — background tokio task polling
//! [`lunaris_core::StoragePort::queue_depth`] every 10 s and updating the
//! `lunaris_verify_queue_depth` + `lunaris_consolidator_queue_depth`
//! Prometheus gauges.
//!
//! ## Pattern source
//!
//! Mirrors `crates/lunaris-verify/src/worker.rs:104-141` shutdown-notify+select
//! shape verbatim — the interval timer replaces the stream subscribe; the
//! shutdown-notify branch stays identical so graceful drain on SIGTERM keeps
//! working through the poller too.
//!
//! ## NotSupported handling
//!
//! Older backends without a `queue_depth` impl return
//! `Err(StorageError::NotSupported(_))` from the trait's default body. The
//! poller recognizes this, leaves the gauge at 0, and emits ONE `tracing::warn!`
//! per topic for the lifetime of the process (rate-limited via a parking_lot
//! `RwLock<bool>` per topic) so log volume stays bounded even on a backend
//! that will never start reporting depths.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use lunaris_core::{StorageError, StoragePort};
use parking_lot::RwLock;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Verify-queue topic — must match `lunaris_verify::worker::VERIFY_TOPIC` and
/// `lunaris/src/ingest.rs::VERIFY_QUEUE_TOPIC`.
pub const VERIFY_TOPIC: &str = "__lunaris_verify__";

/// Consolidator-queue topic — must match
/// `lunaris_consolidate::worker::CONSOLIDATE_TOPIC` and
/// `lunaris/src/ingest.rs::CONSOLIDATE_QUEUE_TOPIC`.
pub const CONSOLIDATE_TOPIC: &str = "__lunaris_consolidate__";

/// Polling interval. CONTEXT.md D-25 says "polled every 10 s".
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Spawn the background queue-depth poller. The returned `JoinHandle` is
/// awaited by the caller during graceful shutdown — a `shutdown.notify_one()`
/// (or `notify_waiters()`) wakes the select branch and exits the loop cleanly.
///
/// The first interval tick fires immediately; we skip it so the gauge is not
/// seeded with a stale 0 before the storage handle has fully initialized.
/// Subsequent ticks fire every [`POLL_INTERVAL`].
pub fn spawn_queue_depth_poller(
    storage: Arc<dyn StoragePort>,
    shutdown: Arc<Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!(
            verify_topic = VERIFY_TOPIC,
            consolidate_topic = CONSOLIDATE_TOPIC,
            interval_secs = POLL_INTERVAL.as_secs(),
            "queue_depth_poller_started"
        );

        let mut tick = tokio::time::interval(POLL_INTERVAL);
        // First tick fires immediately; skip it so the gauge isn't seeded with
        // a value before the storage handle is fully ready.
        tick.tick().await;

        // Per-topic warn-once flags so a backend without queue_depth support
        // doesn't spam the logs every 10 s.
        let warned_verify: Arc<RwLock<bool>> = Arc::new(RwLock::new(false));
        let warned_consolidate: Arc<RwLock<bool>> = Arc::new(RwLock::new(false));

        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    tracing::info!("queue_depth_poller_shutdown_requested; exiting");
                    break;
                }
                _ = tick.tick() => {
                    poll_topic(&storage, VERIFY_TOPIC, &warned_verify, |d| {
                        crate::metrics::metrics()
                            .verify_queue_depth
                            .with_label_values(&[VERIFY_TOPIC])
                            .set(d as i64);
                    })
                    .await;
                    poll_topic(&storage, CONSOLIDATE_TOPIC, &warned_consolidate, |d| {
                        crate::metrics::metrics()
                            .consolidator_queue_depth
                            .with_label_values(&[CONSOLIDATE_TOPIC])
                            .set(d as i64);
                    })
                    .await;
                }
            }
        }

        tracing::info!("queue_depth_poller_exited");
    })
}

/// Poll one topic + apply the gauge update via the supplied closure.
///
/// `NotSupported` → warn ONCE then go quiet (per-topic flag toggled inside
/// the parking_lot RwLock). Other errors warn each cycle — they're transient
/// network blips that operators want to see in real time, not silent failures.
async fn poll_topic<F: FnOnce(u64)>(
    storage: &Arc<dyn StoragePort>,
    topic: &str,
    warned: &Arc<RwLock<bool>>,
    update: F,
) {
    match storage.queue_depth(topic, 0).await {
        Ok(depth) => update(depth),
        Err(StorageError::NotSupported(_)) => {
            // Backend doesn't support queue_depth — warn ONCE then go quiet.
            // CLAUDE.md "never hold a lock across .await" rule: the read is
            // pure CPU; the write that follows is also pure CPU.
            let already = *warned.read();
            if !already {
                *warned.write() = true;
                tracing::warn!(topic, "queue_depth unsupported by backend; gauge stays at 0");
            }
        }
        Err(e) => {
            tracing::warn!(err = %e, topic, "queue_depth call failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_constants_match_canonical_strings() {
        assert_eq!(VERIFY_TOPIC, "__lunaris_verify__");
        assert_eq!(CONSOLIDATE_TOPIC, "__lunaris_consolidate__");
    }

    #[test]
    fn poll_interval_is_ten_seconds() {
        assert_eq!(POLL_INTERVAL, Duration::from_secs(10));
    }
}
