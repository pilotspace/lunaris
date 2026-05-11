//! Plan 04-02 D-04..D-07 + D-16..D-17: in-process tokio worker that consumes
//! `__lunaris_consolidate__`, debounces events, and runs activation passes.
//!
//! ## B-1 audit emit (D-22 verbatim)
//!
//! On every flush, the worker iterates `report.promotions` + `report.archives`
//! and publishes ONE audit message per event. Audit envelope shape matches
//! Plan 04-05 audit.rs `AuditEvent` enum verbatim:
//!
//! - `{"kind": "ConsolidatorPromotion", "episode_id", "fact_id", "activation_score"}`
//! - `{"kind": "ConsolidatorArchive",   "fact_id", "final_activation", "moved_to"}`
//!
//! There is NO rolled-up `ConsolidatorReport` audit envelope — Plan 04-05's
//! `AuditEvent` enum has no such variant. The worker NEVER coalesces multiple
//! promotions or archives into a single audit message.
//!
//! ## W-6 events-by-value spawn boundary
//!
//! `flush()` clones the events vec into the `tokio::spawn` closure by VALUE
//! (`let evts = events.clone(); ... async move { c.consolidate(s, &evts).await }`)
//! so no borrow crosses the spawn boundary. This avoids the lifetime
//! impedance from `Arc<dyn StoragePort>` + `Arc<dyn Consolidator>` borrows.
//!
//! ## Lifecycle
//!
//! ```ignore
//! let shutdown = Arc::new(tokio::sync::Notify::new());
//! let handle = run_consolidate_worker(storage, consolidator, shutdown.clone()).await?;
//! shutdown.notify_one();
//! handle.await.ok();
//! ```
//!
//! ## Debounce (D-17)
//!
//! Multiple `ConsolidateEvent`s for the same `episode_id` within
//! `LUNARIS_CONSOLIDATE_DEBOUNCE_MS` (default 60 000 ms) coalesce into ONE
//! activation-scoring pass. The buffer is a `HashMap<Ulid, Vec<ConsolidateEvent>>`
//! flushed on `tokio::time::sleep_until(deadline)` ticks or on `shutdown`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use lunaris_core::{LunarisError, QueueMsg, StorageError, StoragePort};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;
// Plan 05-05 OPS-05 — `Instrument::instrument` wraps each per-flush body in
// the `lunaris.consolidator_pipeline.process_message` info_span (CONTEXT.md D-24).
use tracing::Instrument;
use ulid::Ulid;

use crate::Consolidator;
use crate::types::{ArchiveEvent, ConsolidateEvent, ConsolidationReport, PromotionEvent};

/// Consumer group name (D-06). Versioned so a v1 message-schema bump can use
/// a fresh consumer group without colliding with running v0 workers.
pub const CONSOLIDATE_CONSUMER_GROUP: &str = "lunaris-consolidate-v0";

/// Consolidate queue topic.
pub const CONSOLIDATE_TOPIC: &str = "__lunaris_consolidate__";

/// Debounce window default (D-17).
const DEFAULT_DEBOUNCE_MS: u64 = 60_000;

/// Env override for [`DEFAULT_DEBOUNCE_MS`].
const ENV_DEBOUNCE_MS: &str = "LUNARIS_CONSOLIDATE_DEBOUNCE_MS";

/// Drain grace-period default (D-07 — mirrors verifier worker contract).
const DEFAULT_DRAIN_MS: u64 = 5000;

/// Env override for [`DEFAULT_DRAIN_MS`].
const ENV_DRAIN_MS: &str = "LUNARIS_WORKER_DRAIN_MS";

/// Spawn the in-process consolidator worker. Returns the `JoinHandle` so the
/// caller (Plan 04-04 `ConsolidatorPipelineHandle::enable()`) can `.await`
/// it during graceful shutdown.
///
/// The worker subscribes to `(lunaris-consolidate-v0,
/// __lunaris_consolidate__, 0)`. Events are debounced into an
/// `episode_id`-keyed buffer; the buffer is flushed every
/// `LUNARIS_CONSOLIDATE_DEBOUNCE_MS` ticks (default 60 s) OR on
/// `shutdown.notify_one()`. Each flush calls `consolidator.consolidate(...)`
/// inside `tokio::spawn` so a panic in the backend never takes down the
/// worker. Per-promotion + per-archive audit messages are emitted to
/// `__lunaris_audit__` after each successful pass (B-1).
pub async fn run_consolidate_worker(
    storage: Arc<dyn StoragePort>,
    consolidator: Arc<dyn Consolidator>,
    shutdown: Arc<Notify>,
    source_prefix: Option<String>,
) -> Result<JoinHandle<()>, LunarisError> {
    // RFC 0001 Wave 0: use Scope::dev() until per-scope queue routing (Wave 3F).
    let stream = storage
        .subscribe(&lunaris_core::Scope::dev(), CONSOLIDATE_CONSUMER_GROUP, CONSOLIDATE_TOPIC, 0)
        .await
        .map_err(LunarisError::Storage)?;

    let debounce_ms = std::env::var(ENV_DEBOUNCE_MS)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DEBOUNCE_MS);
    let drain_ms = std::env::var(ENV_DRAIN_MS)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DRAIN_MS);

    let handle = tokio::spawn(async move {
        tracing::info!(
            consumer_group = CONSOLIDATE_CONSUMER_GROUP,
            topic = CONSOLIDATE_TOPIC,
            debounce_ms,
            drain_ms,
            scope = ?source_prefix,
            "consolidate_worker_started"
        );

        let mut stream = stream;
        let mut buffer: HashMap<Ulid, Vec<ConsolidateEvent>> = HashMap::new();
        let mut next_flush = Instant::now() + Duration::from_millis(debounce_ms);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    tracing::info!(
                        drain_ms,
                        buffered_episodes = buffer.len(),
                        scope = ?source_prefix,
                        "consolidate_worker_shutdown_requested; flushing buffer + draining"
                    );
                    flush(&storage, consolidator.clone(), &mut buffer, source_prefix.as_deref()).await;
                    drain_loop(&mut stream, drain_ms).await;
                    break;
                }
                maybe_msg = stream.next() => {
                    match maybe_msg {
                        None => {
                            tracing::info!("consolidate_worker_stream_closed; flushing + exiting");
                            flush(&storage, consolidator.clone(), &mut buffer, source_prefix.as_deref()).await;
                            break;
                        }
                        Some(Ok(msg)) => {
                            ingest_into_buffer(&mut buffer, msg.payload);
                        }
                        Some(Err(e)) => {
                            tracing::warn!(err = %e, "consolidate_worker_stream_error; continuing");
                            // Backoff so a flapping broker can't tight-loop the CPU
                            // (T-04-02-02 DoS mitigation).
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
                _ = tokio::time::sleep_until(next_flush) => {
                    flush(&storage, consolidator.clone(), &mut buffer, source_prefix.as_deref()).await;
                    next_flush = Instant::now() + Duration::from_millis(debounce_ms);
                }
            }
        }

        tracing::info!(scope = ?source_prefix, "consolidate_worker_exited");
    });

    Ok(handle)
}

/// Drain in-flight messages up to `drain_ms`. Each `stream.next()` call is
/// wrapped in `timeout_at(deadline, ...)` so a stuck broker can't block
/// shutdown past the deadline. We do NOT process the drained messages — the
/// debounce buffer was already flushed by the caller, and the broker will
/// redeliver any unACKed messages on the next worker boot.
async fn drain_loop(
    stream: &mut BoxStream<'static, Result<QueueMsg, StorageError>>,
    drain_ms: u64,
) {
    let deadline = Instant::now() + Duration::from_millis(drain_ms);
    while Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
        }
    }
}

/// Deserialize one queue payload into a [`ConsolidateEvent`] and append it
/// to the `episode_id`-keyed debounce buffer. Malformed payloads are logged
/// + dropped (T-04-02-01 tampering mitigation).
fn ingest_into_buffer(buffer: &mut HashMap<Ulid, Vec<ConsolidateEvent>>, payload: Bytes) {
    match serde_json::from_slice::<ConsolidateEvent>(&payload) {
        Ok(ev) => {
            buffer.entry(ev.episode_id).or_default().push(ev);
        }
        Err(e) => {
            tracing::warn!(err = %e, "consolidate_worker_payload_deserialize_failed; dropping");
        }
    }
}

/// Flush the debounce buffer through `consolidator.consolidate(...)` and
/// publish per-event audit records. Empty buffer → no-op.
///
/// W-6 fix: the events vec is cloned into the `tokio::spawn` closure BY
/// VALUE so no borrow crosses the spawn boundary —
/// `let evts = events.clone(); ... async move { c.consolidate(s, &evts).await }`.
async fn flush(
    storage: &Arc<dyn StoragePort>,
    consolidator: Arc<dyn Consolidator>,
    buffer: &mut HashMap<Ulid, Vec<ConsolidateEvent>>,
    source_prefix: Option<&str>,
) {
    if buffer.is_empty() {
        return;
    }

    // Plan 05-05 OPS-05 — `lunaris.consolidator_pipeline.process_message`
    // per-flush span (CONTEXT.md D-24). The "message" here is the debounced
    // batch of consolidate events keyed by episode_id; one span per flush
    // captures the whole pass (ACT-R scoring + audit emit). `correlation_id`
    // reserved as `tracing::field::Empty` for upstream propagation when the
    // ingest path that emitted the events is in-process.
    let buffered_episodes = buffer.len();
    let total_events: usize = buffer.values().map(|v| v.len()).sum();
    let span = tracing::info_span!(
        "lunaris.consolidator_pipeline.process_message",
        correlation_id = tracing::field::Empty,
        buffered_episodes,
        total_events,
    );
    async move {
        // NoopConsolidator short-circuit — applies()==false means iterate the
        // buffer but never call consolidate (saves the spawn + the clone).
        if !consolidator.applies() {
            tracing::debug!(buffered_episodes, "consolidate_worker_noop_flush; clearing buffer");
            buffer.clear();
            return;
        }

        let events: Vec<ConsolidateEvent> =
            buffer.values().flat_map(|v| v.iter().cloned()).collect();
        buffer.clear();

        // W-6 fix: clone everything into the spawn closure by value. No borrow
        // crosses the spawn boundary.
        //
        // Phase 12 HELIOS-04 — delegate to `Consolidator::consolidate_scoped`
        // so the trait's default filter (Phase 9.1 Plan 01) applies the hard
        // `event.source.starts_with(prefix)` predicate BEFORE promotion. When
        // `source_prefix == None` the default impl forwards the full batch to
        // `consolidate(...)` unchanged — v0.1.0 semantic parity preserved.
        let evts = events.clone();
        let c = consolidator.clone();
        let s = storage.clone();
        let scope: Option<String> = source_prefix.map(|p| p.to_string());
        let report =
            match tokio::spawn(
                async move { c.consolidate_scoped(s, &evts, scope.as_deref()).await },
            )
            .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    tracing::warn!(err = %e, "consolidate_worker_returned_error; events dropped");
                    return;
                }
                Err(join_err) => {
                    tracing::error!(err = %join_err, "consolidate_worker_panicked; loop continues");
                    return;
                }
            };

        publish_per_event_audits(storage, &report).await;
    }
    .instrument(span)
    .await
}

/// B-1 fix: emit ONE audit message per promotion + ONE per archive
/// (D-22 verbatim — matches Plan 04-05 audit.rs `AuditEvent::ConsolidatorPromotion`
/// and `AuditEvent::ConsolidatorArchive` variants exactly).
///
/// `report.communities_rebuilt` is logged via `tracing::info!` only — there
/// is no `AuditEvent::ConsolidatorRebuild` variant in v0 (deferred per the
/// plan's threat model + D-22 audit shape).
async fn publish_per_event_audits(storage: &Arc<dyn StoragePort>, report: &ConsolidationReport) {
    for p in &report.promotions {
        let _ = lunaris_core::audit::publish_audit_event(storage, promotion_event(p)).await;
    }
    for a in &report.archives {
        let _ = lunaris_core::audit::publish_audit_event(storage, archive_event(a)).await;
    }
    if report.communities_rebuilt > 0 {
        tracing::info!(
            communities_rebuilt = report.communities_rebuilt,
            "consolidate_worker_communities_rebuilt"
        );
    }
}

/// Build the per-promotion typed audit event. Plan 13-01 RELEASE-02: the
/// previous inline JSON envelope was replaced by the typed
/// `AuditEvent::ConsolidatorPromotion` variant from `lunaris-core::audit`.
/// The v0.1.0 wire shape is preserved (see
/// `crates/lunaris-core/tests/fixtures/audit/v0.1.0/consolidator_promotion.json`).
fn promotion_event(p: &PromotionEvent) -> lunaris_core::audit::AuditEvent {
    lunaris_core::audit::AuditEvent::ConsolidatorPromotion {
        episode_id: p.episode_id,
        fact_id: lunaris_core::audit::FactIdData(p.fact_id.0),
        activation_score: p.activation_score,
    }
}

/// Build the per-archive typed audit event. See `promotion_event` for the
/// refactor context.
fn archive_event(a: &ArchiveEvent) -> lunaris_core::audit::AuditEvent {
    lunaris_core::audit::AuditEvent::ConsolidatorArchive {
        fact_id: lunaris_core::audit::FactIdData(a.fact_id.0),
        final_activation: a.final_activation,
        moved_to: a.moved_to.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoopConsolidator;
    use crate::types::{ConsolidateEvent, FactId};
    use lunaris_extract::EntityId;

    #[test]
    fn consumer_group_and_topic_are_versioned() {
        assert_eq!(CONSOLIDATE_CONSUMER_GROUP, "lunaris-consolidate-v0");
        assert_eq!(CONSOLIDATE_TOPIC, "__lunaris_consolidate__");
    }

    #[test]
    fn debounce_default_is_60_seconds() {
        assert_eq!(DEFAULT_DEBOUNCE_MS, 60_000);
        assert_eq!(ENV_DEBOUNCE_MS, "LUNARIS_CONSOLIDATE_DEBOUNCE_MS");
    }

    #[test]
    fn drain_ms_default_matches_verifier_worker() {
        assert_eq!(DEFAULT_DRAIN_MS, 5000);
        assert_eq!(ENV_DRAIN_MS, "LUNARIS_WORKER_DRAIN_MS");
    }

    #[test]
    fn audit_topic_matches_d22_contract() {
        assert_eq!(lunaris_core::audit::AUDIT_TOPIC, "__lunaris_audit__");
    }

    #[test]
    fn ingest_into_buffer_groups_by_episode_id() {
        let mut buf: HashMap<Ulid, Vec<ConsolidateEvent>> = HashMap::new();
        let ep = Ulid::new();
        let payload1 = serde_json::to_vec(&ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: ep,
            lsn_wall_ms: 1,
            lsn_counter: 1,
            source: String::new(),
        })
        .unwrap();
        let payload2 = serde_json::to_vec(&ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: ep,
            lsn_wall_ms: 2,
            lsn_counter: 2,
            source: String::new(),
        })
        .unwrap();
        let payload_other = serde_json::to_vec(&ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: Ulid::new(),
            lsn_wall_ms: 3,
            lsn_counter: 3,
            source: String::new(),
        })
        .unwrap();
        ingest_into_buffer(&mut buf, payload1.into());
        ingest_into_buffer(&mut buf, payload2.into());
        ingest_into_buffer(&mut buf, payload_other.into());
        assert_eq!(buf.len(), 2, "two distinct episode_ids");
        assert_eq!(
            buf.get(&ep).map(|v| v.len()),
            Some(2),
            "two events for the same episode coalesce"
        );
    }

    #[test]
    fn ingest_into_buffer_handles_malformed_payload() {
        let mut buf: HashMap<Ulid, Vec<ConsolidateEvent>> = HashMap::new();
        ingest_into_buffer(&mut buf, Bytes::from(&b"not json"[..]));
        assert!(buf.is_empty(), "malformed payload must NOT panic + must NOT enter buffer");
    }

    #[test]
    fn worker_signature_accepts_dyn_consolidator() {
        fn _check(_c: Arc<dyn Consolidator>) {}
        _check(Arc::new(NoopConsolidator));
    }

    /// B-1: promotion envelope `kind` tag matches D-22 / Plan 04-05 audit.rs
    /// verbatim. Plan 13-01 RELEASE-02: now asserts through the typed
    /// `AuditEvent::ConsolidatorPromotion` by round-tripping through serde.
    #[test]
    fn promotion_envelope_kind_is_consolidator_promotion() {
        let p = PromotionEvent {
            episode_id: Ulid::new(),
            fact_id: FactId::from_triple(EntityId([1; 16]), "knows", EntityId([2; 16])),
            activation_score: 1.5,
        };
        let env = serde_json::to_value(promotion_event(&p)).unwrap();
        assert_eq!(
            env.get("kind").and_then(|v: &serde_json::Value| v.as_str()),
            Some("ConsolidatorPromotion")
        );
        assert!(env.get("episode_id").is_some());
        assert!(env.get("fact_id").is_some());
        assert_eq!(
            env.get("activation_score").and_then(|v: &serde_json::Value| v.as_f64()),
            Some(1.5)
        );
    }

    /// B-1: archive envelope `kind` tag matches D-22 verbatim (Plan 13-01
    /// typed variant).
    #[test]
    fn archive_envelope_kind_is_consolidator_archive() {
        let a = ArchiveEvent {
            fact_id: FactId::from_triple(EntityId([1; 16]), "knows", EntityId([2; 16])),
            final_activation: -0.7,
            moved_to: "cold_storage".to_string(),
        };
        let env = serde_json::to_value(archive_event(&a)).unwrap();
        assert_eq!(
            env.get("kind").and_then(|v: &serde_json::Value| v.as_str()),
            Some("ConsolidatorArchive")
        );
        assert!(env.get("fact_id").is_some());
        assert_eq!(
            env.get("final_activation").and_then(|v: &serde_json::Value| v.as_f64()),
            Some(-0.7)
        );
        assert_eq!(
            env.get("moved_to").and_then(|v: &serde_json::Value| v.as_str()),
            Some("cold_storage")
        );
    }

    /// Phase 12 HELIOS-04 — proves `flush()` forwards the `source_prefix`
    /// argument into `Consolidator::consolidate_scoped` and that the filter
    /// drops non-matching events BEFORE they reach `consolidate()`. This is
    /// the worker-level gate for T-12-02-02 (no cross-tenant audit leak).
    #[tokio::test]
    async fn worker_flush_with_scope_prefix_filters_non_matching_events() {
        use async_trait::async_trait;
        use parking_lot::Mutex;

        #[derive(Default)]
        struct RecordingConsolidator {
            seen_scope: Mutex<Option<String>>,
            filtered_event_count: Mutex<usize>,
        }

        #[async_trait]
        impl Consolidator for RecordingConsolidator {
            async fn consolidate(
                &self,
                _storage: Arc<dyn StoragePort>,
                events: &[ConsolidateEvent],
            ) -> Result<ConsolidationReport, LunarisError> {
                *self.filtered_event_count.lock() = events.len();
                Ok(ConsolidationReport::default())
            }

            async fn consolidate_scoped(
                &self,
                storage: Arc<dyn StoragePort>,
                events: &[ConsolidateEvent],
                scope_prefix: Option<&str>,
            ) -> Result<ConsolidationReport, LunarisError> {
                *self.seen_scope.lock() = scope_prefix.map(|s| s.to_string());
                match scope_prefix {
                    None => self.consolidate(storage, events).await,
                    Some(prefix) => {
                        let filtered: Vec<ConsolidateEvent> = events
                            .iter()
                            .filter(|e| e.source.starts_with(prefix))
                            .cloned()
                            .collect();
                        self.consolidate(storage, &filtered).await
                    }
                }
            }

            fn applies(&self) -> bool {
                true
            }
        }

        // Minimal never-panics StoragePort — flush only touches publish()
        // when the report has promotions/archives; RecordingConsolidator
        // returns an empty report, so publish is never reached.
        #[derive(Default)]
        struct NoopStorage;

        #[async_trait]
        impl StoragePort for NoopStorage {
            async fn atomic_write(
                &self,
                _scope: &lunaris_core::Scope,
                _ops: &[lunaris_core::WriteOp],
            ) -> Result<lunaris_core::Lsn, lunaris_core::StorageError> {
                Ok(lunaris_core::Lsn { wall_ms: 0, counter: 0 })
            }
            async fn vector_search(
                &self,
                _scope: &lunaris_core::Scope,
                _i: &str,
                _q: &[f32],
                _k: usize,
                _f: Option<&lunaris_core::Filter>,
                _a: Option<lunaris_core::Hlc>,
                _r: bool,
            ) -> Result<Vec<lunaris_core::VectorHit>, lunaris_core::StorageError> {
                Ok(Vec::new())
            }
            async fn graph_traverse(
                &self,
                _scope: &lunaris_core::Scope,
                _q: &lunaris_core::CypherQuery,
                _a: Option<lunaris_core::Hlc>,
            ) -> Result<lunaris_core::GraphResult, lunaris_core::StorageError> {
                Ok(lunaris_core::GraphResult::default())
            }
            async fn scan_range(
                &self,
                _scope: &lunaris_core::Scope,
                _p: &[u8],
                _a: Option<lunaris_core::Hlc>,
            ) -> Result<
                BoxStream<'_, Result<(Bytes, Bytes), lunaris_core::StorageError>>,
                lunaris_core::StorageError,
            > {
                Ok(Box::pin(futures::stream::iter(Vec::new())))
            }
            async fn read_as_of(
                &self,
                _scope: &lunaris_core::Scope,
                _k: &[u8],
                _a: lunaris_core::Hlc,
            ) -> Result<Option<lunaris_core::Row<Bytes>>, lunaris_core::StorageError> {
                Ok(None)
            }
            async fn publish(
                &self,
                _scope: &lunaris_core::Scope,
                _t: &str,
                _p: u16,
                _payload: Bytes,
            ) -> Result<u64, lunaris_core::StorageError> {
                Ok(0)
            }
            async fn subscribe(
                &self,
                _scope: &lunaris_core::Scope,
                _g: &str,
                _t: &str,
                _p: u16,
            ) -> Result<
                BoxStream<'static, Result<QueueMsg, lunaris_core::StorageError>>,
                lunaris_core::StorageError,
            > {
                Ok(Box::pin(futures::stream::empty()))
            }
            fn capabilities(&self) -> lunaris_core::StorageCapabilities {
                lunaris_core::StorageCapabilities {
                    bi_temporal_native: false,
                    graph_native: false,
                    rerank_native: false,
                    queue_native: false,
                    max_vector_dim: 768,
                    native_rrf: false,
            max_scopes_recommended: 0,
                }
            }
        }

        let rec = Arc::new(RecordingConsolidator::default());
        let c: Arc<dyn Consolidator> = rec.clone();
        let s: Arc<dyn StoragePort> = Arc::new(NoopStorage);

        let mut buf: HashMap<Ulid, Vec<ConsolidateEvent>> = HashMap::new();
        let helios = ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: Ulid::new(),
            lsn_wall_ms: 1,
            lsn_counter: 1,
            source: "helios:fs/session-1/note-0".into(),
        };
        let other = ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: Ulid::new(),
            lsn_wall_ms: 2,
            lsn_counter: 2,
            source: "other:tenant-X/note".into(),
        };
        buf.entry(helios.episode_id).or_default().push(helios);
        buf.entry(other.episode_id).or_default().push(other);

        flush(&s, c, &mut buf, Some("helios:fs/")).await;

        assert_eq!(
            *rec.seen_scope.lock(),
            Some("helios:fs/".to_string()),
            "flush MUST forward source_prefix into consolidate_scoped"
        );
        assert_eq!(
            *rec.filtered_event_count.lock(),
            1,
            "only helios:fs/ events must survive the filter; other: events \
             MUST be dropped BEFORE consolidate() sees them"
        );
    }

    /// Phase 12 HELIOS-04 parity — `source_prefix == None` (v0.1.0 path)
    /// passes the full batch through unchanged to `consolidate(...)`.
    #[tokio::test]
    async fn worker_flush_with_none_prefix_preserves_full_batch() {
        use async_trait::async_trait;
        use parking_lot::Mutex;

        #[derive(Default)]
        struct CountingConsolidator {
            seen_count: Mutex<usize>,
        }

        #[async_trait]
        impl Consolidator for CountingConsolidator {
            async fn consolidate(
                &self,
                _storage: Arc<dyn StoragePort>,
                events: &[ConsolidateEvent],
            ) -> Result<ConsolidationReport, LunarisError> {
                *self.seen_count.lock() = events.len();
                Ok(ConsolidationReport::default())
            }
            fn applies(&self) -> bool {
                true
            }
        }

        #[derive(Default)]
        struct NullStorage;

        #[async_trait]
        impl StoragePort for NullStorage {
            async fn atomic_write(
                &self,
                _scope: &lunaris_core::Scope,
                _ops: &[lunaris_core::WriteOp],
            ) -> Result<lunaris_core::Lsn, lunaris_core::StorageError> {
                Ok(lunaris_core::Lsn { wall_ms: 0, counter: 0 })
            }
            async fn vector_search(
                &self,
                _scope: &lunaris_core::Scope,
                _i: &str,
                _q: &[f32],
                _k: usize,
                _f: Option<&lunaris_core::Filter>,
                _a: Option<lunaris_core::Hlc>,
                _r: bool,
            ) -> Result<Vec<lunaris_core::VectorHit>, lunaris_core::StorageError> {
                Ok(Vec::new())
            }
            async fn graph_traverse(
                &self,
                _scope: &lunaris_core::Scope,
                _q: &lunaris_core::CypherQuery,
                _a: Option<lunaris_core::Hlc>,
            ) -> Result<lunaris_core::GraphResult, lunaris_core::StorageError> {
                Ok(lunaris_core::GraphResult::default())
            }
            async fn scan_range(
                &self,
                _scope: &lunaris_core::Scope,
                _p: &[u8],
                _a: Option<lunaris_core::Hlc>,
            ) -> Result<
                BoxStream<'_, Result<(Bytes, Bytes), lunaris_core::StorageError>>,
                lunaris_core::StorageError,
            > {
                Ok(Box::pin(futures::stream::iter(Vec::new())))
            }
            async fn read_as_of(
                &self,
                _scope: &lunaris_core::Scope,
                _k: &[u8],
                _a: lunaris_core::Hlc,
            ) -> Result<Option<lunaris_core::Row<Bytes>>, lunaris_core::StorageError> {
                Ok(None)
            }
            async fn publish(
                &self,
                _scope: &lunaris_core::Scope,
                _t: &str,
                _p: u16,
                _payload: Bytes,
            ) -> Result<u64, lunaris_core::StorageError> {
                Ok(0)
            }
            async fn subscribe(
                &self,
                _scope: &lunaris_core::Scope,
                _g: &str,
                _t: &str,
                _p: u16,
            ) -> Result<
                BoxStream<'static, Result<QueueMsg, lunaris_core::StorageError>>,
                lunaris_core::StorageError,
            > {
                Ok(Box::pin(futures::stream::empty()))
            }
            fn capabilities(&self) -> lunaris_core::StorageCapabilities {
                lunaris_core::StorageCapabilities {
                    bi_temporal_native: false,
                    graph_native: false,
                    rerank_native: false,
                    queue_native: false,
                    max_vector_dim: 768,
                    native_rrf: false,
            max_scopes_recommended: 0,
                }
            }
        }

        let rec = Arc::new(CountingConsolidator::default());
        let c: Arc<dyn Consolidator> = rec.clone();
        let s: Arc<dyn StoragePort> = Arc::new(NullStorage);

        let mut buf: HashMap<Ulid, Vec<ConsolidateEvent>> = HashMap::new();
        for i in 0..3u32 {
            let ev = ConsolidateEvent {
                kind: "ingest_committed".into(),
                episode_id: Ulid::new(),
                lsn_wall_ms: i as u64,
                lsn_counter: i as u64,
                source: format!("anything:{i}"),
            };
            buf.entry(ev.episode_id).or_default().push(ev);
        }

        flush(&s, c, &mut buf, None).await;

        assert_eq!(
            *rec.seen_count.lock(),
            3,
            "source_prefix=None MUST pass the full batch to consolidate() unchanged (v0.1.0 parity)"
        );
    }

    /// B-1 regression guard: report shape carries per-event vectors, NOT
    /// rolled-up counts. The worker iterates these vectors to emit one audit
    /// message per event.
    #[test]
    fn report_carries_per_event_vectors_not_rolled_up_counts() {
        let r = ConsolidationReport {
            promotions: vec![PromotionEvent {
                episode_id: Ulid::new(),
                fact_id: FactId::from_triple(EntityId([1; 16]), "p", EntityId([2; 16])),
                activation_score: 2.0,
            }],
            archives: vec![ArchiveEvent {
                fact_id: FactId::from_triple(EntityId([3; 16]), "p", EntityId([4; 16])),
                final_activation: -1.0,
                moved_to: "cold".into(),
            }],
            communities_rebuilt: 0,
        };
        assert_eq!(r.promotions.len(), 1);
        assert_eq!(r.archives.len(), 1);
    }
}
