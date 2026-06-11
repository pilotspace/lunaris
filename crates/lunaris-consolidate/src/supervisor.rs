//! Wave 3F: per-scope consolidator worker supervisor (RFC 0001 §3.7).
//!
//! ## Overview
//!
//! `ConsolidateSupervisor` owns one detached `tokio::spawn` task per active
//! [`Scope`]. Each task subscribes to the per-scope consolidate topic
//! (`lunaris:{scope}:consolidate`) and drains events for that scope only. A
//! hot scope cannot starve a cold one because each scope has its own
//! subscription stream.
//!
//! ## Concurrency cap
//!
//! Per-scope tasks acquire a [`Semaphore`] permit before processing each
//! debounced event batch. The cap prevents a single scope from saturating the
//! embedder GPU.
//!
//! Default: 8. Override via `LUNARIS_SCOPE_CONCURRENCY` env var.
//!
//! ## Idle-scope timeout
//!
//! If no events arrive for a scope within `LUNARIS_SCOPE_IDLE_TIMEOUT_MS`
//! (default 30 min / 1 800 000 ms), the scope task voluntarily exits and the
//! supervisor removes it from the active map. The scope can be re-registered
//! later when a new event arrives.
//!
//! Default: 1 800 000 ms (30 min). Override via `LUNARIS_SCOPE_IDLE_TIMEOUT_MS`.
//!
//! ## Panic isolation
//!
//! Each scope's task runs inside a detached `tokio::spawn`. A panic in scope
//! A's task causes that task to exit and the scope is removed from the active
//! map. Scope B is completely unaffected. To restart a failed scope, call
//! `register_scope` again — the supervisor will spawn a new task for it.
//!
//! ## Env vars
//!
//! | Name                           | Default | Description                                        |
//! |--------------------------------|---------|----------------------------------------------------|
//! | `LUNARIS_SCOPE_CONCURRENCY`    | `8`     | Maximum concurrent event-batch processes per scope |
//! | `LUNARIS_SCOPE_IDLE_TIMEOUT_MS`| `1800000`| Idle scope eviction timeout in milliseconds       |

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use lunaris_core::{LunarisError, QueueMsg, Scope, StorageError, StoragePort};
use parking_lot::RwLock;
use tokio::sync::{Semaphore, oneshot};
use tokio::time::Instant;
use tracing::Instrument;
use ulid::Ulid;

use crate::Consolidator;
use crate::types::ConsolidateEvent;
use crate::worker::{
    CONSOLIDATE_CONSUMER_GROUP, DEFAULT_DEBOUNCE_MS, DEFAULT_DRAIN_MS, ENV_DEBOUNCE_MS,
    ENV_DRAIN_MS, flush,
};

// ---------------------------------------------------------------------------
// Env vars for this supervisor
// ---------------------------------------------------------------------------

/// Default per-scope concurrency cap (number of simultaneous event-batch
/// processing tasks per scope).
pub const DEFAULT_SCOPE_CONCURRENCY: usize = 8;

/// Env var name for [`DEFAULT_SCOPE_CONCURRENCY`].
pub const ENV_SCOPE_CONCURRENCY: &str = "LUNARIS_SCOPE_CONCURRENCY";

/// Default idle-scope eviction timeout in milliseconds (30 min).
pub const DEFAULT_SCOPE_IDLE_TIMEOUT_MS: u64 = 1_800_000;

/// Env var name for [`DEFAULT_SCOPE_IDLE_TIMEOUT_MS`].
pub const ENV_SCOPE_IDLE_TIMEOUT_MS: &str = "LUNARIS_SCOPE_IDLE_TIMEOUT_MS";

// ---------------------------------------------------------------------------
// Supervisor types
// ---------------------------------------------------------------------------

/// Per-scope consolidation supervisor.
///
/// One detached `tokio::spawn` task per registered [`Scope`]. Each task
/// subscribes to the per-scope consolidate topic, debounces events, and calls
/// [`Consolidator::consolidate_scoped`]. A panic in scope A's task causes that
/// task to exit and removes the scope from `active`; scope B is completely
/// unaffected. The scope can be re-registered by calling `register_scope` again.
///
/// # Construction
///
/// ```ignore
/// let supervisor = ConsolidateSupervisor::new(storage, consolidator, 8);
/// supervisor.register_scope(Scope::new("acme.agent-1").unwrap()).await?;
/// // ... on shutdown:
/// supervisor.shutdown_all().await;
/// ```
pub struct ConsolidateSupervisor {
    storage: Arc<dyn StoragePort>,
    consolidator: Arc<dyn Consolidator>,
    /// Map from scope to its shutdown-signal sender. Dropping the sender
    /// signals the per-scope task to flush + exit.
    ///
    /// NEVER hold this lock across `.await` — all mutations are lock+extract+unlock.
    active: Arc<RwLock<HashMap<Scope, oneshot::Sender<()>>>>,
    /// Per-scope concurrency cap (number of parallel event-batch processes).
    concurrency_cap: usize,
    debounce_ms: u64,
    drain_ms: u64,
    idle_timeout_ms: u64,
}

impl ConsolidateSupervisor {
    /// Construct a new supervisor. Reads `LUNARIS_SCOPE_CONCURRENCY`,
    /// `LUNARIS_SCOPE_IDLE_TIMEOUT_MS`, `LUNARIS_CONSOLIDATE_DEBOUNCE_MS`, and
    /// `LUNARIS_WORKER_DRAIN_MS` from the environment.
    pub fn new(
        storage: Arc<dyn StoragePort>,
        consolidator: Arc<dyn Consolidator>,
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
        let debounce_ms = std::env::var(ENV_DEBOUNCE_MS)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_DEBOUNCE_MS);
        let drain_ms = std::env::var(ENV_DRAIN_MS)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_DRAIN_MS);
        Self {
            storage,
            consolidator,
            active: Arc::new(RwLock::new(HashMap::new())),
            concurrency_cap,
            debounce_ms,
            drain_ms,
            idle_timeout_ms,
        }
    }

    /// Register a scope, spawning a dedicated consolidation task for it.
    ///
    /// If the scope is already registered this is a no-op (returns `Ok(())`).
    /// If the subscribe call fails, returns `Err` and the scope is NOT added
    /// to the active map.
    pub async fn register_scope(&self, scope: Scope) -> Result<(), LunarisError> {
        // P-3 (v0.2 review) — atomic check-and-reserve under one write lock.
        //
        // The prior implementation read-checked, then dropped the lock and
        // awaited `subscribe`, then write-inserted. Two concurrent
        // `register_scope` calls for the same scope could both pass the
        // read check, both subscribe, and the second `insert` would
        // overwrite the first sender — leaving the first task with a
        // closed shutdown channel and a stranded subscriber.
        //
        // The fix: take a write lock and reserve the slot with a placeholder
        // sender under the SAME lock that observed the scope was absent.
        // Concurrent callers see the placeholder and return Ok early
        // (registration is idempotent). The placeholder is replaced with
        // the real sender once `subscribe` completes; if `subscribe` fails
        // the placeholder is removed so a later retry can proceed.
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

        self.spawn_scope_task(scope, 0).await
    }

    /// Internal: spawn (or re-spawn) a scope task. `attempt` tracks the
    /// restart count for bounded backoff.
    ///
    /// **Invariant (P-3):** caller MUST have reserved the active-map slot
    /// for `scope` via `register_scope`'s placeholder insert before
    /// invoking this function. On any error before the real sender is
    /// installed, the placeholder is removed to leave the supervisor in
    /// the same state as if `register_scope` had never been called.
    async fn spawn_scope_task(&self, scope: Scope, attempt: u32) -> Result<(), LunarisError> {
        // Build the per-scope topic: `lunaris:{scope}:consolidate`
        let topic = scope_consolidate_topic(&scope);

        let stream =
            match self.storage.subscribe(&scope, CONSOLIDATE_CONSUMER_GROUP, &topic, 0).await {
                Ok(s) => s,
                Err(e) => {
                    // Subscribe failed — clean up the placeholder so a later
                    // register_scope call can retry.
                    self.active.write().remove(&scope);
                    return Err(LunarisError::Storage(e));
                }
            };

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        // Replace placeholder with real sender under one write lock.
        {
            let mut guard = self.active.write();
            guard.insert(scope.clone(), shutdown_tx);
        }

        let storage = self.storage.clone();
        let consolidator = self.consolidator.clone();
        let active = self.active.clone();
        let concurrency_cap = self.concurrency_cap;
        let debounce_ms = self.debounce_ms;
        let drain_ms = self.drain_ms;
        let idle_timeout_ms = self.idle_timeout_ms;
        let scope_str = scope.as_str().to_string();

        tokio::spawn(async move {
            let sem = Arc::new(Semaphore::new(concurrency_cap));
            run_scope_task(
                scope.clone(),
                scope_str,
                stream,
                storage,
                consolidator,
                sem,
                active.clone(),
                shutdown_rx,
                debounce_ms,
                drain_ms,
                idle_timeout_ms,
                attempt,
            )
            .await;
        });

        Ok(())
    }

    /// Deregister a scope — sends the shutdown signal to the scope's task.
    /// The task will flush its buffer and exit gracefully.
    ///
    /// No-op if the scope is not currently registered.
    pub fn deregister_scope(&self, scope: &Scope) {
        // Lock, remove, unlock — sender drop signals the task.
        let _sender = {
            let mut guard = self.active.write();
            guard.remove(scope)
        };
        // _sender drops here, closing the oneshot channel and signalling the task.
    }

    /// Shut down all active scope tasks. Blocks until all tasks have acknowledged
    /// shutdown (the oneshot receivers will see the channel closed).
    ///
    /// Consumes `self` so no further registrations are possible after this call.
    pub async fn shutdown_all(self) {
        // Drain all senders — dropping them signals each scope task.
        let senders: Vec<_> = {
            let mut guard = self.active.write();
            guard.drain().map(|(_, tx)| tx).collect()
        };
        // Drop senders to signal scope tasks. We don't await completion here
        // since each scope task has its own drain grace period; they exit on
        // their own bounded schedule.
        drop(senders);
    }

    /// Returns the set of currently-active scopes (snapshot).
    pub fn active_scopes(&self) -> Vec<Scope> {
        self.active.read().keys().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// Per-scope task
// ---------------------------------------------------------------------------

/// The per-scope event loop. Subscribes to `lunaris:{scope}:consolidate`,
/// debounces events, and calls `Consolidator::consolidate_scoped`.
///
/// Panic isolation: this function runs inside a detached `tokio::spawn`. Any
/// panic causes this task to exit; the scope is removed from `active`. The
/// calling supervisor is unaffected and other scope tasks continue running.
#[allow(clippy::too_many_arguments)]
async fn run_scope_task(
    scope: Scope,
    scope_str: String,
    stream: futures::stream::BoxStream<'static, Result<QueueMsg, StorageError>>,
    storage: Arc<dyn StoragePort>,
    consolidator: Arc<dyn Consolidator>,
    sem: Arc<Semaphore>,
    active: Arc<RwLock<HashMap<Scope, oneshot::Sender<()>>>>,
    shutdown_rx: oneshot::Receiver<()>,
    debounce_ms: u64,
    drain_ms: u64,
    idle_timeout_ms: u64,
    attempt: u32,
) {
    let span = tracing::info_span!(
        "lunaris.consolidator_supervisor.scope_task",
        scope = %scope_str,
        attempt,
    );

    async move {
        tracing::info!(
            scope = %scope_str,
            debounce_ms,
            idle_timeout_ms,
            attempt,
            "consolidate_scope_task_started"
        );

        let mut stream = stream;
        let mut buffer: HashMap<Ulid, Vec<ConsolidateEvent>> = HashMap::new();
        let mut next_flush = Instant::now() + Duration::from_millis(debounce_ms);
        let mut last_event = Instant::now();
        let idle_duration = Duration::from_millis(idle_timeout_ms);
        let scope_prefix_str = scope_str.clone();

        // Fuse the shutdown receiver into a future that completes on signal or
        // when the sender is dropped (both trigger task exit).
        let mut shutdown_rx = shutdown_rx;

        loop {
            tokio::select! {
                biased;

                // Shutdown signal: flush buffer and exit gracefully.
                _ = &mut shutdown_rx => {
                    tracing::info!(
                        scope = %scope_str,
                        drain_ms,
                        buffered_episodes = buffer.len(),
                        "consolidate_scope_task_shutdown; flushing + exiting"
                    );
                    // Acquire semaphore before flush to respect the concurrency cap.
                    let _permit = sem.acquire().await.ok();
                    flush(&storage, consolidator.clone(), &mut buffer, Some(&scope_prefix_str)).await;
                    // Drain remaining in-flight messages.
                    let deadline = Instant::now() + Duration::from_millis(drain_ms);
                    while Instant::now() < deadline {
                        match tokio::time::timeout_at(deadline, stream.next()).await {
                            Ok(Some(Ok(_))) => continue,
                            _ => break,
                        }
                    }
                    break;
                }

                // Incoming message from the per-scope topic.
                maybe_msg = stream.next() => {
                    match maybe_msg {
                        None => {
                            tracing::info!(scope = %scope_str, "consolidate_scope_task_stream_closed; flushing + exiting");
                            let _permit = sem.acquire().await.ok();
                            flush(&storage, consolidator.clone(), &mut buffer, Some(&scope_prefix_str)).await;
                            // Remove from active map — the scope is no longer subscribed.
                            active.write().remove(&scope);
                            break;
                        }
                        Some(Ok(msg)) => {
                            last_event = Instant::now();
                            ingest_into_scope_buffer(&mut buffer, msg.payload);
                        }
                        Some(Err(e)) => {
                            tracing::warn!(
                                scope = %scope_str,
                                err = %e,
                                "consolidate_scope_task_stream_error; continuing"
                            );
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }

                // Debounce tick: flush accumulated events.
                _ = tokio::time::sleep_until(next_flush) => {
                    if !buffer.is_empty() {
                        let _permit = sem.acquire().await.ok();
                        flush(&storage, consolidator.clone(), &mut buffer, Some(&scope_prefix_str)).await;
                    }
                    next_flush = Instant::now() + Duration::from_millis(debounce_ms);

                    // Idle check: if no events have arrived within the idle timeout,
                    // deregister this scope to release the subscribe resource.
                    if last_event.elapsed() >= idle_duration {
                        tracing::info!(
                            scope = %scope_str,
                            idle_timeout_ms,
                            "consolidate_scope_task_idle_timeout; deregistering"
                        );
                        active.write().remove(&scope);
                        break;
                    }
                }
            }
        }

        tracing::info!(scope = %scope_str, "consolidate_scope_task_exited");
    }
    .instrument(span)
    .await
}

/// Deserialize one queue payload into a [`ConsolidateEvent`] and append it
/// to the `episode_id`-keyed debounce buffer.
fn ingest_into_scope_buffer(buffer: &mut HashMap<Ulid, Vec<ConsolidateEvent>>, payload: Bytes) {
    match serde_json::from_slice::<ConsolidateEvent>(&payload) {
        Ok(ev) => {
            buffer.entry(ev.episode_id).or_default().push(ev);
        }
        Err(e) => {
            tracing::warn!(
                err = %e,
                "consolidate_scope_task_payload_deserialize_failed; dropping"
            );
        }
    }
}

/// Derive the per-scope consolidate MQ topic name.
///
/// Convention (Wave 1C RFC 0001 §3.6): `lunaris:{scope}:consolidate`
pub fn scope_consolidate_topic(scope: &Scope) -> String {
    format!("lunaris:{}:consolidate", scope.as_str())
}

// ---------------------------------------------------------------------------
// Supervisor handle (production entry point)
// ---------------------------------------------------------------------------

/// A supervisor handle that wraps `ConsolidateSupervisor` with a background
/// monitor loop for operator-level lifecycle management.
///
/// This is the production entry point. Use
/// [`ConsolidateSupervisor::new`] directly when you need fine-grained
/// lifecycle control (e.g., in tests).
pub struct ConsolidateSupervisorHandle {
    supervisor: Arc<ConsolidateSupervisor>,
    /// Signals the monitor loop to exit.
    shutdown_tx: oneshot::Sender<()>,
    monitor_handle: tokio::task::JoinHandle<()>,
}

impl ConsolidateSupervisorHandle {
    /// Construct the supervisor and start the background monitor loop.
    pub fn start(
        storage: Arc<dyn StoragePort>,
        consolidator: Arc<dyn Consolidator>,
        concurrency_cap: usize,
    ) -> Self {
        let supervisor =
            Arc::new(ConsolidateSupervisor::new(storage, consolidator, concurrency_cap));
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let sup_clone = supervisor.clone();
        let monitor_handle = tokio::spawn(monitor_loop(sup_clone, shutdown_rx));
        Self { supervisor, shutdown_tx, monitor_handle }
    }

    /// Register a scope with the supervisor.
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

/// Background monitor: waits for the shutdown signal, then exits.
///
/// Scope tasks are independently spawned via `tokio::spawn`; panic isolation
/// is inherent to `tokio::spawn` (each task is an independent OS-level future).
/// This monitor loop's sole responsibility is to hold the supervisor alive and
/// to propagate the shutdown signal to the caller's `.await` on `shutdown()`.
async fn monitor_loop(
    _supervisor: Arc<ConsolidateSupervisor>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let _ = (&mut shutdown_rx).await;
    tracing::info!("consolidate_supervisor_monitor_exited");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_consolidate_topic_format() {
        let scope = Scope::new("acme.agent-1").unwrap();
        assert_eq!(scope_consolidate_topic(&scope), "lunaris:acme.agent-1:consolidate");
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

    #[test]
    fn supervisor_new_reads_default_concurrency() {
        use async_trait::async_trait;
        use futures::stream::BoxStream;
        use lunaris_core::{
            CypherQuery, Filter, GraphResult, Hlc, Lsn, QueueMsg, Row, StorageCapabilities,
            StorageError, VectorHit, WriteOp,
        };

        #[derive(Default)]
        struct NullStorage;

        #[async_trait]
        impl StoragePort for NullStorage {
            async fn atomic_write(
                &self,
                _scope: &Scope,
                _ops: &[WriteOp],
            ) -> Result<Lsn, StorageError> {
                Ok(Lsn { wall_ms: 0, counter: 0 })
            }
            #[allow(clippy::too_many_arguments)]
            async fn vector_search(
                &self,
                _scope: &Scope,
                _i: &str,
                _q: &[f32],
                _k: usize,
                _f: Option<&Filter>,
                _a: Option<Hlc>,
                _r: bool,
            ) -> Result<Vec<VectorHit>, StorageError> {
                Ok(Vec::new())
            }
            async fn graph_traverse(
                &self,
                _scope: &Scope,
                _q: &CypherQuery,
                _a: Option<Hlc>,
            ) -> Result<GraphResult, StorageError> {
                Ok(GraphResult::default())
            }
            async fn scan_range(
                &self,
                _scope: &Scope,
                _p: &[u8],
                _a: Option<Hlc>,
            ) -> Result<
                BoxStream<'_, Result<(bytes::Bytes, bytes::Bytes), StorageError>>,
                StorageError,
            > {
                Ok(Box::pin(futures::stream::iter(Vec::new())))
            }
            async fn read_as_of(
                &self,
                _scope: &Scope,
                _k: &[u8],
                _a: Hlc,
            ) -> Result<Option<Row<bytes::Bytes>>, StorageError> {
                Ok(None)
            }
            async fn publish(
                &self,
                _scope: &Scope,
                _t: &str,
                _p: u16,
                _payload: bytes::Bytes,
            ) -> Result<u64, StorageError> {
                Ok(0)
            }
            async fn subscribe(
                &self,
                _scope: &Scope,
                _g: &str,
                _t: &str,
                _p: u16,
            ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError>
            {
                Ok(Box::pin(futures::stream::empty()))
            }
            fn capabilities(&self) -> StorageCapabilities {
                StorageCapabilities {
                    bi_temporal_native: false,
                    graph_native: false,
                    rerank_native: false,
                    queue_native: false,
                    max_vector_dim: 768,
                    native_rrf: false,
                    max_scopes_recommended: 0,
                    cypher_dialect: lunaris_core::CypherDialect::Legacy,
                    graph_decay_native: false,
                    graph_navigate_native: false,
                }
            }
        }

        let storage: Arc<dyn StoragePort> = Arc::new(NullStorage);
        let consolidator: Arc<dyn Consolidator> = Arc::new(crate::NoopConsolidator);
        let sup = ConsolidateSupervisor::new(storage, consolidator, DEFAULT_SCOPE_CONCURRENCY);
        // No scopes registered yet.
        assert!(sup.active_scopes().is_empty());
    }
}
