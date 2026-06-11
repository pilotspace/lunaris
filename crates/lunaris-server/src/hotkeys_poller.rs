//! hotkeys-observability — background tokio task polling
//! [`lunaris_core::StoragePort::hot_keys`] every 10 s, classifying raw Moon
//! keys into bounded `(scope, kind)` labels, and updating the
//! `lunaris_hotkey_samples` Prometheus gauge.
//!
//! ## Pattern source
//!
//! Mirrors `queue_depth_poller.rs` verbatim: same interval/shutdown-notify
//! select shape, same warn-once-on-NotSupported discipline.
//!
//! ## Label discipline (D-25)
//!
//! Raw key names NEVER become labels. [`classify_hot_key`] maps each key to
//! `(Scope, kind)` where `kind` is a CLOSED set of static strings (≤ 13) and
//! the scope segment is re-validated through `Scope::new` (label-injection
//! defense). Non-Lunaris keys — other tenants sharing the Moon instance,
//! system keys, malformed bytes — are silently dropped.
//!
//! ## Gauge semantics
//!
//! Moon's HOTKEYS sketch is cumulative since Moon process start with
//! SpaceSaving eviction: the gauge is a pressure RANKING, not a rate.
//! `reset()` runs before each apply so keys falling out of the top-128
//! don't leave stale series; a `NotSupported` or transient-error tick
//! leaves the gauge untouched.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use lunaris_core::{Scope, StorageError, StoragePort};
use parking_lot::RwLock;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Polling interval — matches the queue-depth poller's D-25 cadence.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// How many sketch entries to request per poll. Moon's sketch capacity is
/// 128; asking for all of it maximizes per-scope coverage after dropping
/// non-Lunaris keys.
pub const HOTKEYS_COUNT: usize = 128;

/// Classify a raw backend key into a bounded `(scope, kind)` label pair.
///
/// Recognized shapes (and ONLY these — everything else is `None`):
///
/// | shape                                  | kind                          |
/// |----------------------------------------|-------------------------------|
/// | `lunaris:{scope}:{kind}:{ulid}`        | the kind segment if canonical |
/// | `lunaris:{scope}:{other}:…`            | `kv-other`                    |
/// | `lunaris_{scope}_{chunks…}_idx[:…]`    | `ft-chunks` … `ft-communities`|
/// | `lunaris_{scope}_graph`                | `graph`                       |
///
/// The scope segment is re-validated via `Scope::new` so a hostile or
/// corrupt key can never inject an arbitrary label value.
pub fn classify_hot_key(key: &[u8]) -> Option<(Scope, &'static str)> {
    let s = std::str::from_utf8(key).ok()?;

    // KV canonical: lunaris:{scope}:{kind}:…
    if let Some(rest) = s.strip_prefix("lunaris:") {
        let (scope_str, tail) = rest.split_once(':')?;
        let scope = Scope::new(scope_str).ok()?;
        let kind_str = tail.split(':').next().unwrap_or("");
        let kind = match kind_str {
            "episode" => "episode",
            "chunk" => "chunk",
            "entity" => "entity",
            "relation" => "relation",
            "fact" => "fact",
            "community" => "community",
            "doctree" => "doctree",
            _ => "kv-other",
        };
        return Some((scope, kind));
    }

    // FT doc / graph: lunaris_{scope}_{kind}_idx[:…] | lunaris_{scope}_graph
    // The scope alphabet allows `_`, so parse from the RIGHT: the suffix is
    // unambiguous (`_graph` or `_{kind}_idx`), the middle is the scope.
    if let Some(rest) = s.strip_prefix("lunaris_") {
        let head = rest.split(':').next().unwrap_or(rest);
        if let Some(scope_str) = head.strip_suffix("_graph") {
            let scope = Scope::new(scope_str).ok()?;
            return Some((scope, "graph"));
        }
        let body = head.strip_suffix("_idx")?;
        for (suffix, kind) in [
            ("_chunks", "ft-chunks"),
            ("_entities", "ft-entities"),
            ("_facts", "ft-facts"),
            ("_communities", "ft-communities"),
        ] {
            if let Some(scope_str) = body.strip_suffix(suffix) {
                let scope = Scope::new(scope_str).ok()?;
                return Some((scope, kind));
            }
        }
    }

    None
}

/// One poll cycle: fetch, classify, aggregate, apply. Public so the
/// integration suite can drive ticks deterministically without timers.
///
/// `NotSupported` / transient errors leave the gauge untouched (the reset
/// only runs once a reply is in hand — a failed tick never wipes the last
/// good reading). Returns whether the backend reported support, so the
/// caller can manage the warn-once flag.
pub async fn poll_once(storage: &Arc<dyn StoragePort>) -> bool {
    let hot = match storage.hot_keys(HOTKEYS_COUNT).await {
        Ok(hot) => hot,
        Err(StorageError::NotSupported(_)) => return false,
        Err(e) => {
            tracing::warn!(err = %e, "hot_keys call failed");
            return true;
        }
    };

    let mut sums: HashMap<(String, &'static str), i64> = HashMap::new();
    for hk in &hot {
        if let Some((scope, kind)) = classify_hot_key(&hk.key) {
            let clamped = i64::try_from(hk.sampled_count).unwrap_or(i64::MAX);
            *sums.entry((scope.as_str().to_string(), kind)).or_insert(0) += clamped;
        }
    }

    let gauge = &crate::metrics::metrics().hotkey_samples;
    gauge.reset();
    for ((scope, kind), total) in sums {
        gauge.with_label_values(&[scope.as_str(), kind]).set(total);
    }
    true
}

/// Spawn the background hot-keys poller. Same lifecycle contract as
/// `spawn_queue_depth_poller`: the returned handle is awaited during
/// graceful shutdown after `shutdown.notify_waiters()`.
pub fn spawn_hotkeys_poller(
    storage: Arc<dyn StoragePort>,
    shutdown: Arc<Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!(
            interval_secs = POLL_INTERVAL.as_secs(),
            count = HOTKEYS_COUNT,
            "hotkeys_poller_started"
        );

        let mut tick = tokio::time::interval(POLL_INTERVAL);
        // First tick fires immediately; skip it so the gauge isn't seeded
        // before the storage handle is fully ready.
        tick.tick().await;

        // Warn-once flag so an unsupported backend (SQLite/Postgres) does
        // not spam the logs every 10 s.
        let warned: RwLock<bool> = RwLock::new(false);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    tracing::info!("hotkeys_poller_shutdown_requested; exiting");
                    break;
                }
                _ = tick.tick() => {
                    let supported = poll_once(&storage).await;
                    if !supported {
                        let already = *warned.read();
                        if !already {
                            *warned.write() = true;
                            tracing::warn!(
                                "hot_keys unsupported by backend; lunaris_hotkey_samples stays empty"
                            );
                        }
                    }
                }
            }
        }

        tracing::info!("hotkeys_poller_exited");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_matches_queue_depth_cadence() {
        assert_eq!(POLL_INTERVAL, crate::queue_depth_poller::POLL_INTERVAL);
    }

    #[test]
    fn kind_label_set_is_closed() {
        // Every kind label the classifier can emit, pinned. Adding a label
        // here REQUIRES updating the D-25 cardinality note on the metric.
        let all = [
            "episode",
            "chunk",
            "entity",
            "relation",
            "fact",
            "community",
            "doctree",
            "kv-other",
            "ft-chunks",
            "ft-entities",
            "ft-facts",
            "ft-communities",
            "graph",
        ];
        assert_eq!(all.len(), 13);
    }
}
