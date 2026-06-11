//! `memory.scratchpad_consolidate` — on-demand ACT-R consolidation drain.
//!
//! Guarded by three circuit-breakers (see threat model):
//!   1. `queue_native` gate — fails fast on sqlite / memory://
//!   2. `is_enabled` guard — refuses when background worker is live (races on __mq_consumers)
//!   3. Hard wall-clock timeout — bounds the ~51s worst-case drain
//!
//! Scope is server-bound (from `AppState::scope`). Wire payloads MUST NOT supply or
//! override scope — CLAUDE.md DTO discipline.

use std::time::Duration;

use lunaris::WorkingMemory;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use crate::state::AppState;
use crate::tools::ToolError;

/// Hard wall-clock cap for one consolidate drain (5s).
/// Bounds the ~51s worst-case (DRAIN_CAP=1024 × PULL_TIMEOUT_MS=50ms).
const CONSOLIDATE_TOOL_TIMEOUT: Duration = Duration::from_secs(5);

/// Session-handover consolidate (scratchpad-handover task): the same three
/// guards as the tool handler, around the WHOLE-SCOPE drain
/// (`consolidate_unfiltered` — a prefix filter would drop drained events
/// belonging to other namespaces, see task consolidate-prefix-drop).
///
/// Warn-and-continue by design: handover failures and guard refusals must
/// NEVER error the tool call that triggered them — the old pad carries
/// forward and the handover is retried at the next observed switch.
pub(crate) async fn run_handover_consolidate(state: &AppState) {
    // Guard 1: queue_native gate.
    if !state.lunaris.storage().capabilities().queue_native {
        tracing::warn!(
            scope = state.scope.as_str(),
            "session handover: backend has no native queue — skipping consolidate, \
             previous pad carries forward",
        );
        return;
    }
    // Guard 2: background worker refusal (single __mq_consumers group).
    if state.lunaris.consolidator_pipeline().is_enabled() {
        tracing::warn!(
            scope = state.scope.as_str(),
            "session handover: background consolidation worker is live — skipping \
             consolidate to avoid double-consume, previous pad carries forward",
        );
        return;
    }
    // The namespace on this WorkingMemory is irrelevant to the unfiltered
    // drain; pass the legacy default for the audit trail.
    let wm =
        WorkingMemory::new(state.lunaris.clone(), state.scope.clone(), "scratchpad/".to_owned());
    // Guard 3: hard timeout.
    match timeout(CONSOLIDATE_TOOL_TIMEOUT, wm.consolidate_unfiltered()).await {
        Err(_elapsed) => tracing::warn!(
            scope = state.scope.as_str(),
            "session handover: consolidate exceeded the hard timeout — partial \
             progress possible, retrying at the next switch",
        ),
        Ok(Err(e)) => tracing::warn!(
            scope = state.scope.as_str(), err = %e,
            "session handover: consolidate failed — previous pad carries forward",
        ),
        Ok(Ok(report)) => tracing::info!(
            scope = state.scope.as_str(),
            promotions = report.promotions.len(),
            archives = report.archives.len(),
            "session handover: previous session's pending events consolidated",
        ),
    }
}

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.scratchpad_consolidate`.
///
/// `#[serde(deny_unknown_fields)]` mandatory (CLAUDE.md §HTTP DTO discipline).
/// `scope` is absent — bound at server startup, never on the wire.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScratchpadConsolidateParams {
    /// Optional source-key namespace prefix (default: "scratchpad/").
    /// Charset: [A-Za-z0-9_\-./]{1..=128}. ':' is rejected.
    /// Scopes the consolidation to events matching this prefix.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// Result of `memory.scratchpad_consolidate`.
///
/// FLAT STRUCT, **not** a `#[serde(tag = ...)]` enum: the generated MCP
/// `outputSchema` must have a root `type: "object"`, but a tagged enum's schema
/// root is `oneOf` (no `type`), which rmcp 1.7 rejects — aborting server startup
/// for ALL builds. See `tests::response_outputschema_root_is_object`. The
/// `status` field carries the outcome discriminator; `message` is present only
/// for non-`ok` statuses (wire shape for `ok` is unchanged from the old enum:
/// `{status:"ok",promotions,archives}`).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct ScratchpadConsolidateResponse {
    /// Outcome discriminator: "ok" | "unsupported_backend" | "worker_conflict" | "timeout".
    pub status: String,
    /// Facts promoted (activation above threshold). 0 unless `status == "ok"`.
    pub promotions: usize,
    /// Facts archived (activation below threshold). 0 unless `status == "ok"`.
    pub archives: usize,
    /// Human-readable detail for non-`ok` statuses; omitted when `status == "ok"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ScratchpadConsolidateResponse {
    /// `status: "ok"` — consolidation ran and produced a report.
    fn ok(promotions: usize, archives: usize) -> Self {
        Self { status: "ok".into(), promotions, archives, message: None }
    }
    /// `status: "unsupported_backend"` — backend has no native queue (guard 1).
    fn unsupported_backend(message: impl Into<String>) -> Self {
        Self {
            status: "unsupported_backend".into(),
            promotions: 0,
            archives: 0,
            message: Some(message.into()),
        }
    }
    /// `status: "worker_conflict"` — background worker is live (guard 2).
    fn worker_conflict(message: impl Into<String>) -> Self {
        Self {
            status: "worker_conflict".into(),
            promotions: 0,
            archives: 0,
            message: Some(message.into()),
        }
    }
    /// `status: "timeout"` — drain exceeded the hard wall-clock cap (guard 3).
    fn timeout(message: impl Into<String>) -> Self {
        Self { status: "timeout".into(), promotions: 0, archives: 0, message: Some(message.into()) }
    }
}

// ── Handler ───────────────────────────────────────────────────────────────────

pub(crate) async fn handle(
    state: &AppState,
    params: ScratchpadConsolidateParams,
) -> Result<ScratchpadConsolidateResponse, ToolError> {
    handle_inner(state, params, CONSOLIDATE_TOOL_TIMEOUT).await
}

/// Injectable-timeout variant for tests (allows sub-ms timeout in test contexts).
pub(crate) async fn handle_inner(
    state: &AppState,
    params: ScratchpadConsolidateParams,
    timeout_dur: Duration,
) -> Result<ScratchpadConsolidateResponse, ToolError> {
    // Guard 1: queue_native gate (circuit-breaker — fail fast on non-Moon backends)
    if !state.lunaris.storage().capabilities().queue_native {
        return Ok(ScratchpadConsolidateResponse::unsupported_backend(
            "consolidate requires a native-queue backend (Moon); \
             current backend has no durable queue (queue_native=false). \
             Start lunaris-mcp with the embedded-moon feature or point \
             --storage to a moon:// URL.",
        ));
    }

    // Guard 2: background worker refusal (prevents double-consume on __mq_consumers)
    if state.lunaris.consolidator_pipeline().is_enabled() {
        return Ok(ScratchpadConsolidateResponse::worker_conflict(
            "background consolidation worker is live — calling \
             memory.scratchpad_consolidate now would race on the single \
             __mq_consumers consumer group and silently lose events. \
             Disable the worker (LUNARIS_CONSOLIDATOR=0) before using \
             this tool.",
        ));
    }

    let namespace = crate::tools::staging::resolve_namespace(params.namespace)?;
    let wm = WorkingMemory::new(state.lunaris.clone(), state.scope.clone(), namespace);

    // Guard 3: hard timeout (bounds the ~51s worst-case DRAIN_CAP × PULL_TIMEOUT_MS drain)
    match timeout(timeout_dur, wm.consolidate()).await {
        Err(_elapsed) => Ok(ScratchpadConsolidateResponse::timeout(format!(
            "consolidate drain exceeded {}s hard timeout; partial progress may \
             have occurred — retry to continue draining remaining events.",
            timeout_dur.as_secs()
        ))),
        Ok(Err(e)) => Err(ToolError::LunarisEngine(e)),
        Ok(Ok(report)) => {
            Ok(ScratchpadConsolidateResponse::ok(report.promotions.len(), report.archives.len()))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_core::{Scope, StubEmbedder};

    use super::*;
    use crate::state::AppState;

    // ── Fixtures ──────────────────────────────────────────────────────────────

    /// memory:// fixture — queue_native=false, no Moon needed.
    /// Mirrors scratchpad_write.rs::fresh_state exactly.
    async fn fresh_state_memory(scope_name: &str) -> AppState {
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Lunaris::open_with_embedder("memory://", embedder).await.unwrap();
        let scope = Scope::new(scope_name).unwrap();
        AppState {
            lunaris: Arc::new(lunaris),
            scope,
            #[cfg(feature = "embedded-moon")]
            _embedded_moon: None,
        }
    }

    /// embedded-moon fixture — queue_native=true, unique data dir per test via tempfile.
    /// enable() is NOT called here — pipeline.is_enabled() stays false.
    /// TempDir ownership: the caller MUST hold the returned TempDir for the duration
    /// of the test (declare it before AppState so it drops after AppState).
    #[cfg(feature = "embedded-moon")]
    async fn fresh_state_moon(scope_name: &str, data_dir: &str) -> AppState {
        // Constructs AppState via the PRODUCTION bootstrap path (bootstrap_inner),
        // so set_consolidator is called and ActR is installed — same wiring as production.
        // skip_probe=true avoids requiring real GGUF weights in CI.
        AppState::bootstrap_inner(Some(scope_name), None, true, Some(data_dir)).await.unwrap()
    }

    // ── Schema-validity regression ────────────────────────────────────────────

    /// REGRESSION (codex dogfood, 2026-06-09): the response type's generated MCP
    /// `outputSchema` MUST have a root `type: "object"`. rmcp 1.7 validates this
    /// when building the tool router and ABORTS server startup otherwise — which a
    /// `#[serde(tag = "status")]` enum triggered (its schema root is `oneOf` with
    /// no `type`), making `lunaris-mcp` un-launchable for ALL builds despite green
    /// unit tests (they call `handle()` directly and never build the rmcp router).
    /// This test reproduces rmcp's validation at the schema level — RED on the
    /// tagged enum, GREEN on the flat struct.
    #[test]
    fn response_outputschema_root_is_object() {
        let schema = schemars::schema_for!(ScratchpadConsolidateResponse);
        let v = serde_json::to_value(&schema).expect("schema serializes to JSON");
        assert_eq!(
            v.get("type").and_then(|t| t.as_str()),
            Some("object"),
            "MCP outputSchema root must be type:object — rmcp 1.7 aborts startup otherwise; got: {v}"
        );
    }

    // ── Guard tests ───────────────────────────────────────────────────────────

    /// Guard 1: memory:// backend (queue_native=false) → UnsupportedBackend.
    /// No Moon needed — works on any backend.
    #[tokio::test]
    async fn guard_queue_native_false_returns_unsupported_backend() {
        let state = fresh_state_memory("test-cons-gate").await;
        let resp = handle(&state, ScratchpadConsolidateParams { namespace: None }).await.unwrap();
        assert_eq!(
            resp.status, "unsupported_backend",
            "memory:// must return unsupported_backend; got: {resp:?}"
        );
    }

    /// Guard 2: embedded-moon + pipeline.enable() → WorkerConflict.
    /// Requires queue_native=true so guard 1 passes.
    #[cfg(feature = "embedded-moon")]
    #[tokio::test]
    async fn guard_bg_worker_enabled_returns_worker_conflict() {
        let _tmpdir = tempfile::tempdir().unwrap(); // must outlive state
        let state = fresh_state_moon("test-cons-bgworker", _tmpdir.path().to_str().unwrap()).await;
        // Force-enable the pipeline — makes is_enabled() == true, spawns worker.
        state.lunaris.consolidator_pipeline().enable();
        let resp = handle(&state, ScratchpadConsolidateParams { namespace: None }).await.unwrap();
        assert_eq!(
            resp.status, "worker_conflict",
            "enabled pipeline must return worker_conflict; got: {resp:?}"
        );
    }

    /// Guard 3: embedded-moon + 1ms injectable timeout → Timeout fires.
    ///
    /// The topic must EXIST before the drain polls it; MQ POP on a non-existent
    /// topic returns a non-Array reply which the drain treats as a break signal
    /// (Ok([])) rather than blocking. Seeding one message ensures the topic is
    /// created; after the drain consumes that message, the next MQ POP on the
    /// now-empty topic returns an empty array → Moon's 250ms idle sleep, capped
    /// at PULL_TIMEOUT_MS=50ms. Either way the outer 1ms timeout fires well before
    /// either the 50ms inner cap or the 250ms Moon sleep completes.
    #[cfg(feature = "embedded-moon")]
    #[tokio::test]
    async fn guard_timeout_fires_within_wall_clock_bound() {
        use lunaris_consolidate::{CONSOLIDATE_TOPIC, ConsolidateEvent};
        use ulid::Ulid;

        let _tmpdir = tempfile::tempdir().unwrap(); // must outlive state
        let state = fresh_state_moon("test-cons-timeout", _tmpdir.path().to_str().unwrap()).await;
        let storage = state.lunaris.storage();

        // Seed one message so the topic exists; after it's consumed the next poll
        // blocks on an existing-but-empty topic (the path that sleeps / times out).
        let ev = ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: Ulid::new(),
            lsn_wall_ms: 1000,
            lsn_counter: 1,
            source: "scratchpad/timeout-probe".into(),
        };
        let payload = serde_json::to_vec(&ev).unwrap();
        storage.publish(&state.scope, CONSOLIDATE_TOPIC, 0, payload.into()).await.unwrap();

        let start = std::time::Instant::now();
        let resp = handle_inner(
            &state,
            ScratchpadConsolidateParams { namespace: None },
            std::time::Duration::from_millis(1),
        )
        .await
        .unwrap();
        let elapsed = start.elapsed();
        assert_eq!(resp.status, "timeout", "1ms timeout must fire; got: {resp:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(6),
            "wall-clock must be < 6s; got: {elapsed:?}"
        );
    }

    // ── WIRED discriminating test ─────────────────────────────────────────────

    /// WIRED: seed an epoch-aged ConsolidateEvent under the real server scope,
    /// call handle(), assert non-empty report AND audit event emission.
    ///
    /// This is the discriminating test for the T1a scope-dev fix: the event is
    /// seeded under `state.scope` (not Scope::dev()), so it is only consumed if
    /// drain_consolidate_events subscribes under the real scope.
    ///
    /// Requires embedded-moon (queue_native=true; subscribe works).
    #[cfg(feature = "embedded-moon")]
    #[tokio::test]
    async fn seed_aged_event_produces_non_empty_report() {
        use futures::StreamExt;
        use lunaris_consolidate::{CONSOLIDATE_TOPIC, ConsolidateEvent};
        use lunaris_core::audit::AUDIT_TOPIC;
        use ulid::Ulid;

        let _tmpdir = tempfile::tempdir().unwrap(); // must outlive state
        let state = fresh_state_moon("test-cons-wired", _tmpdir.path().to_str().unwrap()).await;
        let storage = state.lunaris.storage();

        // Seed: lsn_wall_ms=1000 (epoch) → ACT-R activation deeply negative → archives.
        // Scope MUST equal state.scope — discriminating scope-dev-fix check.
        // StoragePort::publish signature: (scope, topic, partition, payload).
        let ev = ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: Ulid::new(),
            lsn_wall_ms: 1000,
            lsn_counter: 1,
            source: "scratchpad/test".into(),
        };
        let payload = serde_json::to_vec(&ev).unwrap();
        storage.publish(&state.scope, CONSOLIDATE_TOPIC, 0, payload.into()).await.unwrap();

        let resp = handle(&state, ScratchpadConsolidateParams { namespace: None }).await.unwrap();

        assert_eq!(resp.status, "ok", "expected ok status; got: {resp:?}");
        let (promotions, archives) = (resp.promotions, resp.archives);
        assert!(
            promotions + archives > 0,
            "seeding under real scope must produce non-empty report (scope-dev fix check); \
             promotions={promotions}, archives={archives}"
        );

        // Audit readback: publish_per_event_audits uses Publisher trait which emits
        // under Scope::dev() (Wave-1E known debt — lunaris_core::audit::Publisher
        // impl hardcodes Scope::dev()). Subscribe to AUDIT_TOPIC under Scope::dev()
        // to verify at least one audit was emitted.
        let mut audit_stream =
            storage.subscribe(&Scope::dev(), "audit-drain-test", AUDIT_TOPIC, 0).await.unwrap();
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(2));
        tokio::pin!(deadline);
        let mut found_audit = false;
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                msg = audit_stream.next() => {
                    match msg {
                        Some(Ok(m)) => {
                            if let Ok(
                                lunaris_core::audit::AuditEvent::ConsolidatorArchive { .. }
                                | lunaris_core::audit::AuditEvent::ConsolidatorPromotion { .. },
                            ) = serde_json::from_slice::<lunaris_core::audit::AuditEvent>(&m.payload)
                            {
                                found_audit = true;
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
        }
        assert!(
            found_audit,
            "at least one ConsolidatorArchive or ConsolidatorPromotion audit must be emitted"
        );
    }
}
