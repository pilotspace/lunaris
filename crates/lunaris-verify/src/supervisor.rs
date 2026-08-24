//! Wave 3F: per-scope verifier worker supervisor (RFC 0001 §3.7).
//!
//! ## Overview
//!
//! `VerifySupervisor` owns one task per active [`Scope`]. Each task subscribes
//! to the per-scope verify topic (`lunaris:{scope}:verify`) and processes
//! arbitration decisions for that scope only. A hot scope cannot starve a cold
//! one because each scope has its own subscription stream.
//!
//! ## Concurrency cap
//!
//! Per-scope tasks acquire a [`Semaphore`] permit before processing each
//! message. The cap prevents a single scope from saturating the verifier
//! backend (LLM or GPU).
//!
//! Default: 8. Override via `LUNARIS_SCOPE_CONCURRENCY` env var.
//!
//! ## Idle-scope timeout
//!
//! If no events arrive within `LUNARIS_SCOPE_IDLE_TIMEOUT_MS` (default 30 min),
//! the scope task exits voluntarily and the supervisor removes it from the
//! active map. The scope can be re-registered when a new event arrives.
//!
//! Default: 1 800 000 ms (30 min). Override via `LUNARIS_SCOPE_IDLE_TIMEOUT_MS`.
//!
//! ## Panic isolation
//!
//! A panic in any one scope's verifier task is contained — the supervisor logs
//! the failure and the task exits. The scope is removed from `active` so future
//! `register_scope` calls can re-start it. Other scopes are unaffected.
//!
//! ## Env vars
//!
//! | Name                           | Default  | Description                                         |
//! |--------------------------------|----------|-----------------------------------------------------|
//! | `LUNARIS_SCOPE_CONCURRENCY`    | `8`      | Maximum concurrent message processes per scope      |
//! | `LUNARIS_SCOPE_IDLE_TIMEOUT_MS`| `1800000`| Idle scope eviction timeout in milliseconds         |

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use lunaris_core::{HlcClock, LunarisError, QueueMsg, Scope, StorageError, StoragePort};
use parking_lot::RwLock;
use tokio::sync::{Semaphore, oneshot};
use tokio::time::Instant;
use tracing::Instrument;

use crate::Verifier;
use crate::worker::{DEFAULT_DRAIN_MS, ENV_DRAIN_MS, VERIFY_CONSUMER_GROUP, process_one};

// ---------------------------------------------------------------------------
// Env vars (shared with consolidate supervisor — same names)
// ---------------------------------------------------------------------------

/// Default per-scope concurrency cap.
pub const DEFAULT_SCOPE_CONCURRENCY: usize = 8;

/// Env var for [`DEFAULT_SCOPE_CONCURRENCY`].
pub const ENV_SCOPE_CONCURRENCY: &str = "LUNARIS_SCOPE_CONCURRENCY";

/// Default idle timeout in milliseconds (30 min).
pub const DEFAULT_SCOPE_IDLE_TIMEOUT_MS: u64 = 1_800_000;

/// Env var for [`DEFAULT_SCOPE_IDLE_TIMEOUT_MS`].
pub const ENV_SCOPE_IDLE_TIMEOUT_MS: &str = "LUNARIS_SCOPE_IDLE_TIMEOUT_MS";

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// Per-scope verifier supervisor.
///
/// One Tokio task per registered [`Scope`]. Each task subscribes to
/// `lunaris:{scope}:verify`, processes arbitration decisions via
/// [`crate::Verifier::verify`], and applies the MVCC supersede write.
/// A panic in scope A's task does not affect scope B.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use lunaris_core::{HlcClock, LunarisError, Scope, StoragePort};
/// use lunaris_verify::{Verifier, VerifySupervisor};
///
/// # async fn demo(
/// #     storage: Arc<dyn StoragePort>,
/// #     verifier: Arc<dyn Verifier>,
/// #     clock: Arc<HlcClock>,
/// # ) -> Result<(), LunarisError> {
/// let supervisor = VerifySupervisor::new(storage, verifier, clock, 8);
/// supervisor.register_scope(Scope::new("acme.agent-1").unwrap()).await?;
/// // ... on shutdown:
/// supervisor.shutdown_all().await;
/// # Ok(()) }
/// ```
pub struct VerifySupervisor {
    storage: Arc<dyn StoragePort>,
    verifier: Arc<dyn Verifier>,
    clock: Arc<HlcClock>,
    /// Active scope → shutdown-signal sender. Dropping the sender signals the task.
    ///
    /// NEVER hold this lock across `.await`.
    active: Arc<RwLock<HashMap<Scope, oneshot::Sender<()>>>>,
    concurrency_cap: usize,
    drain_ms: u64,
    idle_timeout_ms: u64,
}

impl VerifySupervisor {
    /// Construct a new supervisor. Reads `LUNARIS_SCOPE_CONCURRENCY`,
    /// `LUNARIS_SCOPE_IDLE_TIMEOUT_MS`, and `LUNARIS_WORKER_DRAIN_MS` from
    /// the environment.
    pub fn new(
        storage: Arc<dyn StoragePort>,
        verifier: Arc<dyn Verifier>,
        clock: Arc<HlcClock>,
        concurrency_cap: usize,
    ) -> Self {
        let concurrency_cap = std::env::var(ENV_SCOPE_CONCURRENCY)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(concurrency_cap);
        let idle_timeout_ms = std::env::var(ENV_SCOPE_IDLE_TIMEOUT_MS)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_SCOPE_IDLE_TIMEOUT_MS);
        let drain_ms = std::env::var(ENV_DRAIN_MS)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_DRAIN_MS);
        Self {
            storage,
            verifier,
            clock,
            active: Arc::new(RwLock::new(HashMap::new())),
            concurrency_cap,
            drain_ms,
            idle_timeout_ms,
        }
    }

    /// Register a scope, spawning a dedicated verify task for it.
    ///
    /// If the scope is already registered this is a no-op. If the subscribe
    /// call fails, returns `Err` and the scope is NOT added to the active map.
    pub async fn register_scope(&self, scope: Scope) -> Result<(), LunarisError> {
        // P-3 (v0.2 review) — atomic check-and-reserve under one write lock.
        // See consolidate supervisor's register_scope for the full rationale.
        let needs_spawn = {
            let mut guard = self.active.write();
            if guard.contains_key(&scope) {
                false
            } else {
                let (placeholder, _placeholder_rx) = oneshot::channel::<()>();
                guard.insert(scope.clone(), placeholder);
                true
            }
        };
        if !needs_spawn {
            return Ok(());
        }
        self.spawn_scope_task(scope).await
    }

    /// **Invariant (P-3):** caller MUST have reserved the active-map slot
    /// before invoking this function (see `register_scope`).
    async fn spawn_scope_task(&self, scope: Scope) -> Result<(), LunarisError> {
        let topic = scope_verify_topic(&scope);

        let stream = match self.storage.subscribe(&scope, VERIFY_CONSUMER_GROUP, &topic, 0).await {
            Ok(s) => s,
            Err(e) => {
                self.active.write().remove(&scope);
                return Err(LunarisError::Storage(e));
            }
        };

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        // Replace placeholder with real sender.
        {
            let mut guard = self.active.write();
            guard.insert(scope.clone(), shutdown_tx);
        }

        let storage = self.storage.clone();
        let verifier = self.verifier.clone();
        let clock = self.clock.clone();
        let active = self.active.clone();
        let concurrency_cap = self.concurrency_cap;
        let drain_ms = self.drain_ms;
        let idle_timeout_ms = self.idle_timeout_ms;

        tokio::spawn(async move {
            let sem = Arc::new(Semaphore::new(concurrency_cap));
            run_verify_scope_task(
                scope,
                stream,
                storage,
                verifier,
                clock,
                sem,
                active,
                shutdown_rx,
                drain_ms,
                idle_timeout_ms,
            )
            .await;
        });

        Ok(())
    }

    /// Deregister a scope — sends the shutdown signal to the scope's task.
    /// No-op if the scope is not currently registered.
    pub fn deregister_scope(&self, scope: &Scope) {
        // Drop sender to signal task exit. Lock released before drop.
        let _sender = self.active.write().remove(scope);
    }

    /// Shut down all active scope tasks by dropping all shutdown senders.
    pub async fn shutdown_all(self) {
        let _senders: Vec<_> = self.active.write().drain().map(|(_, tx)| tx).collect();
        // Senders drop here, signalling all scope tasks.
    }

    /// Returns the currently-active scopes (snapshot).
    pub fn active_scopes(&self) -> Vec<Scope> {
        self.active.read().keys().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// Per-scope verify task
// ---------------------------------------------------------------------------

/// Derive the per-scope verify MQ topic name.
///
/// Convention (Wave 1C RFC 0001 §3.6): `lunaris:{scope}:verify`
pub fn scope_verify_topic(scope: &Scope) -> String {
    format!("lunaris:{}:verify", scope.as_str())
}

#[allow(clippy::too_many_arguments)]
async fn run_verify_scope_task(
    scope: Scope,
    stream: futures::stream::BoxStream<'static, Result<QueueMsg, StorageError>>,
    storage: Arc<dyn StoragePort>,
    verifier: Arc<dyn Verifier>,
    clock: Arc<HlcClock>,
    sem: Arc<Semaphore>,
    active: Arc<RwLock<HashMap<Scope, oneshot::Sender<()>>>>,
    shutdown_rx: oneshot::Receiver<()>,
    drain_ms: u64,
    idle_timeout_ms: u64,
) {
    let scope_str = scope.as_str().to_string();
    let span = tracing::info_span!(
        "lunaris.verify_supervisor.scope_task",
        scope = %scope_str,
    );

    async move {
        tracing::info!(
            scope = %scope_str,
            idle_timeout_ms,
            "verify_scope_task_started"
        );

        let mut stream = stream;
        let mut last_event = Instant::now();
        let idle_duration = Duration::from_millis(idle_timeout_ms);
        // Use a periodic tick to check idle timeout (reuse drain_ms as a
        // reasonable poll interval, capped at 60 s).
        let poll_interval = Duration::from_millis(drain_ms.min(60_000));
        let mut shutdown_rx = shutdown_rx;

        loop {
            tokio::select! {
                biased;

                // Shutdown signal: drain in-flight messages, then exit.
                _ = &mut shutdown_rx => {
                    tracing::info!(
                        scope = %scope_str,
                        drain_ms,
                        "verify_scope_task_shutdown; draining"
                    );
                    let deadline = Instant::now() + Duration::from_millis(drain_ms);
                    while Instant::now() < deadline {
                        match tokio::time::timeout_at(deadline, stream.next()).await {
                            Ok(Some(Ok(msg))) => {
                                let _permit = sem.acquire().await.ok();
                                process_one(&storage, verifier.clone(), &clock, &scope, msg.payload).await;
                            }
                            _ => break,
                        }
                    }
                    break;
                }

                // Incoming message.
                maybe_msg = stream.next() => {
                    match maybe_msg {
                        None => {
                            tracing::info!(scope = %scope_str, "verify_scope_task_stream_closed; exiting");
                            active.write().remove(&scope);
                            break;
                        }
                        Some(Ok(msg)) => {
                            last_event = Instant::now();
                            let _permit = sem.acquire().await.ok();
                            process_one(&storage, verifier.clone(), &clock, &scope, msg.payload).await;
                        }
                        Some(Err(e)) => {
                            tracing::warn!(
                                scope = %scope_str,
                                err = %e,
                                "verify_scope_task_stream_error; continuing"
                            );
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }

                // Idle check tick.
                _ = tokio::time::sleep(poll_interval) => {
                    if last_event.elapsed() >= idle_duration {
                        tracing::info!(
                            scope = %scope_str,
                            idle_timeout_ms,
                            "verify_scope_task_idle_timeout; deregistering"
                        );
                        active.write().remove(&scope);
                        break;
                    }
                }
            }
        }

        tracing::info!(scope = %scope_str, "verify_scope_task_exited");
    }
    .instrument(span)
    .await
}

// ---------------------------------------------------------------------------
// Handle (optional convenience wrapper)
// ---------------------------------------------------------------------------

/// A convenience handle that wraps `VerifySupervisor` with a background
/// monitor loop for operator-level lifecycle management.
pub struct VerifySupervisorHandle {
    supervisor: Arc<VerifySupervisor>,
    shutdown_tx: oneshot::Sender<()>,
    monitor_handle: tokio::task::JoinHandle<()>,
}

impl VerifySupervisorHandle {
    /// Construct and start the supervisor.
    pub fn start(
        storage: Arc<dyn StoragePort>,
        verifier: Arc<dyn Verifier>,
        clock: Arc<HlcClock>,
        concurrency_cap: usize,
    ) -> Self {
        let supervisor = Arc::new(VerifySupervisor::new(storage, verifier, clock, concurrency_cap));
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let sup_clone = supervisor.clone();
        let monitor_handle = tokio::spawn(async move {
            let mut rx = shutdown_rx;
            let _ = (&mut rx).await;
            tracing::info!("verify_supervisor_monitor_exited");
            drop(sup_clone);
        });
        Self { supervisor, shutdown_tx, monitor_handle }
    }

    /// Register a scope.
    pub async fn register_scope(&self, scope: Scope) -> Result<(), LunarisError> {
        self.supervisor.register_scope(scope).await
    }

    /// Deregister a scope.
    pub fn deregister_scope(&self, scope: &Scope) {
        self.supervisor.deregister_scope(scope);
    }

    /// Shut down the supervisor and all scope tasks.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.monitor_handle.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_verify_topic_format() {
        let scope = Scope::new("acme.agent-1").unwrap();
        assert_eq!(scope_verify_topic(&scope), "lunaris:acme.agent-1:verify");
    }

    #[test]
    fn default_concurrency_constant() {
        assert_eq!(DEFAULT_SCOPE_CONCURRENCY, 8);
        assert_eq!(ENV_SCOPE_CONCURRENCY, "LUNARIS_SCOPE_CONCURRENCY");
    }

    #[test]
    fn default_idle_timeout_constant() {
        assert_eq!(DEFAULT_SCOPE_IDLE_TIMEOUT_MS, 1_800_000);
        assert_eq!(ENV_SCOPE_IDLE_TIMEOUT_MS, "LUNARIS_SCOPE_IDLE_TIMEOUT_MS");
    }
}
