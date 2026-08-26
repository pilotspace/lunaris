//! Plan 04-01 D-04..D-07 + Plan 04-04 Task 4 (B-2): in-process tokio worker
//! that consumes `__lunaris_verify__` and applies arbitration decisions
//! through `StoragePort::atomic_write` with the MVCC supersede contract
//! (D-11).
//!
//! ## Lifecycle
//!
//! ```no_run
//! use std::sync::Arc;
//! use lunaris_core::{LunarisError, StoragePort};
//! use lunaris_verify::{Verifier, run_verify_worker};
//!
//! # async fn demo(
//! #     storage: Arc<dyn StoragePort>,
//! #     verifier: Arc<dyn Verifier>,
//! # ) -> Result<(), LunarisError> {
//! let shutdown = Arc::new(tokio::sync::Notify::new());
//! // `HlcClock::new` already returns an `Arc<HlcClock>`.
//! let clock = lunaris_core::HlcClock::new(0);
//! let handle = run_verify_worker(storage, verifier, shutdown.clone(), clock).await?;
//! shutdown.notify_one();
//! handle.await.ok();
//! # Ok(()) }
//! ```
//!
//! ## B-2 fix — real MVCC primitive-row supersede (Plan 04-04 Task 4)
//!
//! `apply_supersede` loads the actual primitive `Row<Bytes>` for both winner
//! and loser via `read_as_of`, stamps the loser's `bt.sys.1 = Some(now)` via
//! `BiTemporal::invalidate_sys` AND **JSON-patches the mutated bt back into
//! the payload bytes** before emitting the `WriteOp::KvPut`. This mirrors
//! Plan 04-05 `forget.rs::build_soft_delete_op` because `WriteOp::KvPut`
//! carries no separate `bt` field — the Moon HSET layer and Postgres row
//! storage both derive the persisted bt from the serialized payload, so a
//! typed-local mutation that doesn't ride inside the payload bytes is
//! discarded. ONE `atomic_write` per decision (D-11).
//!
//! ## Shutdown + drain (D-07)
//!
//! On `shutdown.notified()` the loop enters a drain phase bounded by the
//! env `LUNARIS_WORKER_DRAIN_MS` (default 5 s). During drain the loop keeps
//! pulling messages but each message is wrapped in `tokio::time::timeout_at`
//! so a stuck verifier cannot hold the shutdown path open past the deadline.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use lunaris_core::QueueMsg;
use lunaris_core::keyspace::{entity_key, fact_key, relation_key};
use lunaris_core::{BiTemporal, HlcClock, LunarisError, Scope, StorageError, StoragePort, WriteOp};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
// Plan 05-05 OPS-05 — `Instrument::instrument` wraps each per-message body
// in the `lunaris.verify_pipeline.process_message` info_span (CONTEXT.md D-24).
use tracing::Instrument;

use crate::Verifier;
use crate::types::VerifyDecision;
use lunaris_extract::NeedsReviewItem;

/// Consumer group name (D-06). Versioned so a v1 message-schema bump can use
/// a fresh consumer group without colliding with running v0 workers.
pub const VERIFY_CONSUMER_GROUP: &str = "lunaris-verify-v0";

/// Verify queue topic (matches Plan 03-03 `ingest.rs::VERIFY_QUEUE_TOPIC`).
pub const VERIFY_TOPIC: &str = "__lunaris_verify__";

/// Default drain grace period (D-07).
pub(crate) const DEFAULT_DRAIN_MS: u64 = 5000;

/// Env override for [`DEFAULT_DRAIN_MS`].
pub(crate) const ENV_DRAIN_MS: &str = "LUNARIS_WORKER_DRAIN_MS";

/// Spawn the in-process verifier worker. Returns the `JoinHandle` so the
/// caller (Plan 04-04 `VerifierPipelineHandle::enable()`) can `.await` it
/// during graceful shutdown.
///
/// The worker subscribes to `(lunaris-verify-v0, __lunaris_verify__, 0)`.
/// On every message it:
///
/// 1. Deserializes the `publish_needs_review` envelope (ingest.rs:341-355).
/// 2. Invokes `verifier.verify(item)` inside `tokio::spawn` so a panic in
///    the backend never takes down the worker — the outer loop continues.
/// 3. If the returned [`VerifyDecision`] `applies()`, calls
///    `apply_supersede` (ONE `atomic_write`; D-11 invariant).
/// 4. On successful supersede, publishes one audit record to
///    `__lunaris_audit__` fire-and-forget (D-22).
///
/// Errors at any step (deserialize / verifier-error / atomic_write /
/// audit-publish) are logged via `tracing::warn!` but do NOT propagate — the
/// broker will redeliver on atomic_write failure; deserialize/audit/verifier
/// failures surface as dropped messages on purpose so a poisoned message
/// doesn't block the queue.
///
/// # Deprecation
///
/// This single-topic entry point is superseded by
/// [`crate::supervisor::VerifySupervisor`] (Wave 3F, RFC 0001 §3.7).
/// It is preserved as a backwards-compatible wrapper that registers one
/// `Scope::dev()` task with the supervisor. New code should construct
/// `VerifySupervisor` directly and register scopes via `register_scope`.
#[deprecated(note = "use VerifySupervisor (crate::supervisor) — Wave 3F RFC 0001 §3.7")]
pub async fn run_verify_worker(
    storage: Arc<dyn StoragePort>,
    verifier: Arc<dyn Verifier>,
    shutdown: Arc<Notify>,
    clock: Arc<HlcClock>,
) -> Result<JoinHandle<()>, LunarisError> {
    // RFC 0001: the verify queue is a global topic shared across scopes.
    // Scope::dev() is intentional here — per-scope verify routing is deferred.
    // scope-dev-allowed: inside-deprecated-wrapper — v0.3 supervisor migration
    // tracked in RFC 0001 §3.7 / docs/v0.3-known-debt.md.
    let dev_scope = Scope::dev();
    let stream = storage
        .subscribe(&dev_scope, VERIFY_CONSUMER_GROUP, VERIFY_TOPIC, 0)
        .await
        .map_err(LunarisError::Storage)?;

    let drain_ms = std::env::var(ENV_DRAIN_MS)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DRAIN_MS);

    // dev_scope is moved into the spawned task for use in process_one /
    // drain_loop calls. The deprecated legacy wrapper keeps Scope::dev() here;
    // VerifySupervisor passes the real scope in its per-scope task.
    let handle = tokio::spawn(async move {
        tracing::info!(
            consumer_group = VERIFY_CONSUMER_GROUP,
            topic = VERIFY_TOPIC,
            "verify_worker_started"
        );

        let scope = dev_scope;
        let mut stream = stream;
        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    tracing::info!(
                        drain_ms,
                        "verify_worker_shutdown_requested; entering drain"
                    );
                    drain_loop(&mut stream, &storage, verifier.clone(), &clock, &scope, drain_ms).await;
                    break;
                }
                maybe_msg = stream.next() => {
                    match maybe_msg {
                        None => {
                            tracing::info!("verify_worker_stream_closed; exiting");
                            break;
                        }
                        Some(Ok(msg)) => {
                            process_one(&storage, verifier.clone(), &clock, &scope, msg.payload).await;
                        }
                        Some(Err(e)) => {
                            tracing::warn!(err = %e, "verify_worker_stream_error; continuing");
                            // Backoff so a flapping broker can't tight-loop the CPU
                            // (T-04-01-02 DoS mitigation).
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        }

        tracing::info!("verify_worker_exited");
    });

    Ok(handle)
}

/// Drain in-flight messages up to `drain_ms`. Each `stream.next()` call is
/// wrapped in `timeout_at(deadline, ...)` so a stuck verifier can't block
/// shutdown past the deadline.
async fn drain_loop(
    stream: &mut BoxStream<'static, Result<QueueMsg, StorageError>>,
    storage: &Arc<dyn StoragePort>,
    verifier: Arc<dyn Verifier>,
    clock: &Arc<HlcClock>,
    scope: &Scope,
    drain_ms: u64,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(drain_ms);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(msg))) => {
                process_one(storage, verifier.clone(), clock, scope, msg.payload).await;
            }
            Ok(Some(Err(e))) => {
                tracing::warn!(err = %e, "verify_worker_drain_stream_error");
                break;
            }
            Ok(None) | Err(_) => break,
        }
    }
}

/// Process one message from the verify queue. Exposed as `pub(crate)` so the
/// per-scope supervisor task can reuse this logic with the real scope instead
/// of `Scope::dev()`.
pub(crate) async fn process_one(
    storage: &Arc<dyn StoragePort>,
    verifier: Arc<dyn Verifier>,
    clock: &Arc<HlcClock>,
    scope: &Scope,
    payload: Bytes,
) {
    // Plan 05-05 OPS-05 — `lunaris.verify_pipeline.process_message` per-message
    // span (CONTEXT.md D-24). `correlation_id` reserved as
    // `tracing::field::Empty`; populated upstream by whatever Lunaris boundary
    // emitted the verify-queue envelope (T-05-05-01: span field shape; the
    // ingest path that emitted the message has its own `lunaris.ingest` parent
    // span, but cross-process queue messages don't auto-propagate without an
    // OpenTelemetry W3C trace-context header — that's a v1 enhancement).
    let span = tracing::info_span!(
        "lunaris.verify_pipeline.process_message",
        correlation_id = tracing::field::Empty,
        payload_bytes = payload.len(),
    );
    async move {
        // NoopVerifier short-circuit — applies()==false means never call verify
        // (T-04-01-07 mitigation: no panic path through a noop backend).
        if !verifier.applies() {
            return;
        }

        let envelope: VerifyEnvelope = match serde_json::from_slice(&payload) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(err = %e, "verify_worker_payload_deserialize_failed; dropping");
                return;
            }
        };

        // Plan 04-04 Task 4 (B-2): keep the envelope's `kind` for the
        // canonical-key derivation in apply_supersede. We move the body fields
        // out of the envelope into the typed NeedsReviewItem next, so we
        // capture `kind` first.
        let envelope_kind = envelope.kind.clone();

        let item = match envelope.into_needs_review() {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(err = %e, "verify_worker_envelope_to_item_failed; dropping");
                return;
            }
        };

        // Inner spawn — a panic inside verifier.verify() bubbles up to the join
        // handle which we observe as Err(JoinError); the outer loop continues
        // (T-04-01-07 verifier-panic-does-not-kill-worker).
        let result = match tokio::spawn({
            let v = verifier.clone();
            async move { v.verify(item).await }
        })
        .await
        {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => {
                tracing::warn!(err = %e, "verify_worker_verifier_returned_error; nack");
                return;
            }
            Err(join_err) => {
                tracing::error!(err = %join_err, "verify_worker_verifier_panicked; loop continues");
                return;
            }
        };

        if !result.applies() {
            return;
        }

        if let Err(e) = apply_supersede(storage, &result, clock, &envelope_kind, scope).await {
            tracing::warn!(err = %e, "verify_worker_atomic_write_failed; broker will redeliver");
            return;
        }

        if let Err(e) = publish_arbitration_audit(storage, scope, &result).await {
            tracing::warn!(
                err = %e,
                "verify_worker_audit_publish_failed; decision committed but audit lost"
            );
        }
    }
    .instrument(span)
    .await
}

/// D-11 MVCC supersede invariant: ONE atomic_write call.
///
/// ## Plan 04-04 Task 4 (B-2 + B-2-RESIDUAL) — real primitive-row supersede
///
/// 1. Derive the canonical key from `envelope_kind` + winner/loser ulid:
///    `entity:<ulid>` / `relation:<ulid>` / `fact:<ulid>` (matching
///    `lunaris/src/ingest.rs` key prefixes).
/// 2. `storage.read_as_of(key, now)` for both winner + loser rows.
/// 3. Mutate the loser's `BiTemporal::sys.1 = Some(now)` via
///    `invalidate_sys` AND **JSON-patch the mutated bt back into the
///    payload bytes** before emitting `WriteOp::KvPut`. This mirrors Plan
///    04-05 `forget.rs::build_soft_delete_op`.
/// 4. Mutate the winner's `BiTemporal { valid: (now, None), sys: (now,
///    None) }` and JSON-patch the same way.
/// 5. ONE storage.atomic_write call with the [loser_op, winner_op] slice.
///
/// **Why the JSON-patch pattern is mandatory (B-2-RESIDUAL):**
/// `WriteOp::KvPut` is `{ key: Vec<u8>, value: Vec<u8> }` — it has NO `bt`
/// field. The Moon HSET layer (`crates/lunaris-storage-moon/src/kv.rs`) and
/// Postgres row storage both derive the persisted bt from the serialized
/// payload, so a typed-local mutation that doesn't ride inside the payload
/// bytes is discarded. Task 4 mirrors the JSON-patch pattern proven in
/// Plan 04-05 forget.rs.
#[doc(hidden)] // exposed for the memory-update-intelligence supersede integration test
pub async fn apply_supersede(
    storage: &Arc<dyn StoragePort>,
    decision: &VerifyDecision,
    clock: &Arc<HlcClock>,
    envelope_kind: &str,
    scope: &Scope,
) -> Result<(), LunarisError> {
    let winner_id = decision.winner_id.expect("checked applies() earlier");
    let loser_id = decision.loser_id.expect("checked applies() earlier");

    // 1. Derive the CANONICAL `lunaris:{scope}:{kind}:{ulid}` keys via the
    //    `lunaris_core::keyspace` helpers — the SAME helpers the ingest write
    //    path uses (`entity_key` / `relation_key` / `fact_key`). Minting a key
    //    from a local `format!` here was a latent bug: it produced the
    //    scope-less `{kind}:{ulid}`, so `read_as_of` (which matches the literal
    //    key) never found the real row and the supersede silently tombstoned a
    //    phantom key — the loser stayed open (memory-update-intelligence
    //    discriminating test `supersede_closes_loser_at_canonical_fact_key`).
    //    CONVENTIONS.md: any caller that mints a KV key from a local helper is
    //    a bug. Unknown kinds still error out (T-04-04-09 mitigation).
    let (winner_key, loser_key) = match envelope_kind {
        "entity" => (entity_key(scope, winner_id), entity_key(scope, loser_id)),
        "relation" => (relation_key(scope, winner_id), relation_key(scope, loser_id)),
        "fact" => (fact_key(scope, winner_id), fact_key(scope, loser_id)),
        other => {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "unknown envelope kind: {other}"
            ))));
        }
    };

    // 2. Stamp `now` via HlcClock::tick (the Hlc type itself has no
    //    `now()` constructor — Hlc::ZERO is the only const, and HlcClock
    //    is the source of monotonic timestamps).
    let now = clock.tick();

    // 3. Load existing rows using the caller-supplied scope (real per-scope
    //    routing — Wave 3F RFC 0001 §3.7 replaces the former Scope::dev() placeholder).
    let winner_existing =
        storage.read_as_of(scope, &winner_key, now).await.map_err(LunarisError::Storage)?;
    let loser_existing =
        storage.read_as_of(scope, &loser_key, now).await.map_err(LunarisError::Storage)?;

    // 4. LOSER WriteOp — invalidate_sys + JSON-patch payload["bt"].
    let loser_op = match loser_existing {
        Some(row) => {
            let mut loser_bt = row.bt;
            loser_bt.invalidate_sys(now);

            let mut payload: serde_json::Value =
                serde_json::from_slice(&row.value).map_err(|e| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "loser payload parse: {e}"
                    )))
                })?;

            payload["bt"] = serde_json::to_value(loser_bt).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!("loser bt serialize: {e}")))
            })?;

            let loser_bytes = serde_json::to_vec(&payload).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "loser payload serialize: {e}"
                )))
            })?;

            WriteOp::KvPut { key: loser_key.clone(), value: loser_bytes }
        }
        None => {
            // No prior loser row — emit a tombstone-shaped payload carrying
            // a bt with sys=(now, Some(now)) so the row is immediately
            // invalidated even though it has no history. JSON-patch shape
            // mirrors the live-row path so the mock + real backends parse
            // it identically.
            let synthetic_bt = BiTemporal { valid: (now, None), sys: (now, Some(now)) };
            let payload = serde_json::json!({
                "verifier_decision": "loser_superseded_no_prior_row",
                "ulid": loser_id.to_string(),
                "superseded_by": winner_id.to_string(),
                "decided_at_iso": decision.decided_at_iso,
                "bt": synthetic_bt,
            });
            let bytes = serde_json::to_vec(&payload).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "loser synthetic serialize: {e}"
                )))
            })?;
            WriteOp::KvPut { key: loser_key.clone(), value: bytes }
        }
    };

    // 5. WINNER WriteOp — stamp bt.sys.0 = now, clear bt.sys.1, reset
    //    bt.valid.0 = now (v0 simplification — if the verifier produces
    //    revised valid bounds in a future VerifyEnvelope extension, plumb
    //    them through here instead of resetting). JSON-patch the same way.
    let winner_op = match winner_existing {
        Some(row) => {
            let mut winner_bt = row.bt;
            winner_bt.sys = (now, None);
            winner_bt.valid = (now, None);

            let mut payload: serde_json::Value =
                serde_json::from_slice(&row.value).map_err(|e| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "winner payload parse: {e}"
                    )))
                })?;

            payload["bt"] = serde_json::to_value(winner_bt).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!("winner bt serialize: {e}")))
            })?;

            let winner_bytes = serde_json::to_vec(&payload).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "winner payload serialize: {e}"
                )))
            })?;

            WriteOp::KvPut { key: winner_key.clone(), value: winner_bytes }
        }
        None => {
            // No prior winner row yet — synthesize one carrying the
            // verifier decision body PLUS a fresh bt. JSON-patch shape
            // matches the live-row path.
            let fresh_bt = BiTemporal { valid: (now, None), sys: (now, None) };
            let payload = serde_json::json!({
                "verifier_decision": "winner",
                "ulid": winner_id.to_string(),
                "reason": decision.reason,
                "decided_at_iso": decision.decided_at_iso,
                "bt": fresh_bt,
            });
            let bytes = serde_json::to_vec(&payload).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "winner synthetic serialize: {e}"
                )))
            })?;
            WriteOp::KvPut { key: winner_key.clone(), value: bytes }
        }
    };

    // 6. ONE atomic_write per decision (D-11 invariant).
    //    Exactly TWO ops in this call: [loser_op, winner_op].
    //    Wave 3F: uses the caller-supplied scope (real per-scope routing).
    storage
        .atomic_write(scope, &[loser_op, winner_op])
        .await
        .map(|_lsn| ())
        .map_err(LunarisError::Storage)
}

/// Publish one `AuditEvent::VerifierArbitration` to `__lunaris_audit__`
/// fire-and-forget (D-22). Plan 13-01 RELEASE-02: the inline
/// `serde_json::json!({ "kind": ... })` envelope construction was removed in
/// favor of the typed helper `lunaris_core::audit::publish_audit_event`.
/// The `backend` string is produced via serde so it stays byte-identical to
/// the v0.1.0 wire shape even if a future `#[serde(rename=...)]` is added to
/// `VerifierBackend`.
async fn publish_arbitration_audit(
    storage: &Arc<dyn StoragePort>,
    scope: &Scope,
    decision: &VerifyDecision,
) -> Result<u64, LunarisError> {
    let backend = serde_json::to_value(decision.backend)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let event = lunaris_core::audit::AuditEvent::VerifierArbitration {
        winner_id: decision.winner_id.map(|u| u.to_string()),
        loser_id: decision.loser_id.map(|u| u.to_string()),
        reason: decision.reason.clone(),
        backend,
        decided_at_iso: decision.decided_at_iso.clone(),
    };
    lunaris_core::audit::publish_audit_event(storage, scope, event)
        .await
        .map_err(|e| LunarisError::Storage(StorageError::Backend(e.to_string())))
}

// ------------------------------- Envelope ----------------------------------

/// Worker-side read of the envelope produced by
/// `lunaris::ingest::publish_needs_review` (ingest.rs:341-355). The envelope
/// tags the `kind` so the worker can pick the right deserialize shape for
/// `item.raw`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct VerifyEnvelope {
    kind: String, // "entity" | "relation" | "fact"
    item: VerifyEnvelopeBody,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct VerifyEnvelopeBody {
    reason: lunaris_extract::NeedsReviewReason,
    raw: serde_json::Value,
}

impl VerifyEnvelope {
    fn into_needs_review(self) -> Result<NeedsReviewItem, String> {
        match self.kind.as_str() {
            "entity" => {
                let raw: lunaris_extract::types::Entity =
                    serde_json::from_value(self.item.raw).map_err(|e| e.to_string())?;
                Ok(NeedsReviewItem::Entity { reason: self.item.reason, raw })
            }
            "relation" => {
                let raw: lunaris_extract::types::Relation =
                    serde_json::from_value(self.item.raw).map_err(|e| e.to_string())?;
                Ok(NeedsReviewItem::Relation { reason: self.item.reason, raw })
            }
            "fact" => {
                let raw: lunaris_extract::types::Fact =
                    serde_json::from_value(self.item.raw).map_err(|e| e.to_string())?;
                Ok(NeedsReviewItem::Fact { reason: self.item.reason, raw })
            }
            other => Err(format!("unknown envelope kind: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoopVerifier;
    use crate::types::VerifierBackend;

    #[test]
    fn drain_ms_env_var_is_read() {
        assert_eq!(DEFAULT_DRAIN_MS, 5000);
        assert_eq!(ENV_DRAIN_MS, "LUNARIS_WORKER_DRAIN_MS");
    }

    #[test]
    fn consumer_group_and_topic_are_versioned() {
        assert_eq!(VERIFY_CONSUMER_GROUP, "lunaris-verify-v0");
        assert_eq!(VERIFY_TOPIC, "__lunaris_verify__");
    }

    #[test]
    fn audit_topic_matches_d22_contract() {
        assert_eq!(lunaris_core::audit::AUDIT_TOPIC, "__lunaris_audit__");
    }

    #[tokio::test]
    async fn deferred_decision_short_circuits_atomic_write() {
        let d = VerifyDecision::deferred();
        assert!(!d.applies());
    }

    #[tokio::test]
    async fn arbitrate_decision_is_apply_eligible() {
        let d = VerifyDecision::arbitrate(
            ulid::Ulid::new(),
            ulid::Ulid::new(),
            "test",
            VerifierBackend::Noop,
        );
        assert!(d.applies());
    }

    #[test]
    fn audit_envelope_shape_is_versioned_kind() {
        // Plan 13-01 RELEASE-02: this test now constructs the typed
        // `AuditEvent::VerifierArbitration` from `lunaris-core::audit`
        // instead of an inline `serde_json::json!` envelope (which the CI
        // grep gate blocks). Kept for regression coverage of the worker's
        // "kind tag" contract.
        let d = VerifyDecision::arbitrate(
            ulid::Ulid::new(),
            ulid::Ulid::new(),
            "winner higher confidence",
            VerifierBackend::CloudAnthropic,
        );
        let backend = serde_json::to_value(d.backend)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let event = lunaris_core::audit::AuditEvent::VerifierArbitration {
            winner_id: d.winner_id.map(|u| u.to_string()),
            loser_id: d.loser_id.map(|u| u.to_string()),
            reason: d.reason.clone(),
            backend,
            decided_at_iso: d.decided_at_iso.clone(),
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value.get("kind").and_then(|v| v.as_str()), Some("VerifierArbitration"));
    }

    #[test]
    fn worker_signature_accepts_dyn_verifier() {
        fn _check(_v: Arc<dyn Verifier>) {}
        _check(Arc::new(NoopVerifier));
    }

    #[test]
    fn envelope_deserializes_entity_kind() {
        let id_bytes: Vec<u8> = vec![0u8; 16];
        let entity_raw = serde_json::json!({
            "id": id_bytes,
            "name": "Alice",
            "aliases": [],
            "entity_type": "Person",
            "confidence": 0.9,
            "valid_from_iso": "2026-01-01T00:00:00Z",
            "valid_to_iso": null,
        });
        let envelope = serde_json::json!({
            "kind": "entity",
            "item": {
                "reason": {
                    "InvalidBitemporal": {
                        "valid_from": "2026-01-01T00:00:00Z",
                        "valid_to": "2025-01-01T00:00:00Z"
                    }
                },
                "raw": entity_raw,
            }
        });
        let parsed: VerifyEnvelope = serde_json::from_value(envelope).expect("deserialize");
        assert_eq!(parsed.kind, "entity");
        let item = parsed.into_needs_review().expect("into_needs_review");
        assert!(matches!(item, NeedsReviewItem::Entity { .. }));
    }

    #[test]
    fn envelope_rejects_unknown_kind() {
        let bad = VerifyEnvelope {
            kind: "unknown".into(),
            item: VerifyEnvelopeBody {
                reason: lunaris_extract::NeedsReviewReason::GbnfFailure {
                    schema_path: "x".into(),
                    error: "y".into(),
                },
                raw: serde_json::json!({}),
            },
        };
        let err = bad.into_needs_review().unwrap_err();
        assert!(err.contains("unknown envelope kind"));
    }
}
