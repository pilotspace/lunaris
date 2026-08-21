//! `memory.scratchpad_consolidate` — on-demand ACT-R consolidation drain.
//!
//! Transport-neutral (scratchpad-proxiable task): moved from
//! `lunaris-mcp/src/tools/scratchpad_consolidate.rs`. Guarded by three
//! circuit-breakers:
//!   1. `queue_native` gate — fails fast on sqlite / memory://
//!   2. `is_enabled` guard — refuses when the background worker is live
//!      (races on the single `__mq_consumers` group)
//!   3. Hard wall-clock timeout — bounds the ~51s worst-case drain

use std::sync::Arc;
use std::time::Duration;

use lunaris::{Lunaris, WorkingMemory};
use lunaris_core::Scope;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use crate::ServiceError;

/// Hard wall-clock cap for one consolidate drain (5s).
/// Bounds the ~51s worst-case (DRAIN_CAP=1024 × PULL_TIMEOUT_MS=50ms).
pub const CONSOLIDATE_TOOL_TIMEOUT: Duration = Duration::from_secs(5);

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.scratchpad_consolidate`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScratchpadConsolidateParams {
    /// Optional source-key namespace prefix (default: "scratchpad/").
    /// Charset: [A-Za-z0-9_\-./]{1..=128}. ':' is rejected.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// Result of `memory.scratchpad_consolidate`.
///
/// FLAT STRUCT, **not** a `#[serde(tag = ...)]` enum: the generated MCP
/// `outputSchema` root must be `type: "object"`, but a tagged enum's schema
/// root is `oneOf` (no `type`), which rmcp 1.7 rejects — aborting server
/// startup for ALL builds. The `status` field carries the discriminator.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScratchpadConsolidateResponse {
    /// Outcome: "ok" | "unsupported_backend" | "worker_conflict" | "timeout".
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

pub async fn handle(
    lunaris: &Arc<Lunaris>,
    scope: &Scope,
    params: ScratchpadConsolidateParams,
) -> Result<ScratchpadConsolidateResponse, ServiceError> {
    handle_inner(lunaris, scope, params, CONSOLIDATE_TOOL_TIMEOUT).await
}

/// Injectable-timeout variant for tests (allows sub-ms timeout in test contexts).
pub async fn handle_inner(
    lunaris: &Arc<Lunaris>,
    scope: &Scope,
    params: ScratchpadConsolidateParams,
    timeout_dur: Duration,
) -> Result<ScratchpadConsolidateResponse, ServiceError> {
    // Guard 1: queue_native gate (fail fast on non-Moon backends).
    if !lunaris.storage().capabilities().queue_native {
        return Ok(ScratchpadConsolidateResponse::unsupported_backend(
            "consolidate requires a native-queue backend (Moon); \
             current backend has no durable queue (queue_native=false). \
             Start lunaris-mcp with the embedded-moon feature or point \
             --storage to a moon:// URL.",
        ));
    }

    // Guard 2: background worker refusal (prevents double-consume on __mq_consumers).
    if lunaris.consolidator_pipeline().is_enabled() {
        return Ok(ScratchpadConsolidateResponse::worker_conflict(
            "background consolidation worker is live — calling \
             memory.scratchpad_consolidate now would race on the single \
             __mq_consumers consumer group and silently lose events. \
             Disable the worker (LUNARIS_CONSOLIDATE_ENABLED=0, read once at \
             Lunaris::open) and restart before using this tool.",
        ));
    }

    let namespace = crate::namespace::resolve(params.namespace)?;
    let wm = WorkingMemory::new(lunaris.clone(), scope.clone(), namespace);

    // Guard 3: hard timeout (bounds the ~51s worst-case drain).
    match timeout(timeout_dur, wm.consolidate()).await {
        Err(_elapsed) => Ok(ScratchpadConsolidateResponse::timeout(format!(
            "consolidate drain exceeded {}s hard timeout; partial progress may \
             have occurred — retry to continue draining remaining events.",
            timeout_dur.as_secs()
        ))),
        Ok(Err(e)) => Err(ServiceError::LunarisEngine(e)),
        Ok(Ok(report)) => {
            Ok(ScratchpadConsolidateResponse::ok(report.promotions.len(), report.archives.len()))
        }
    }
}

// ── Tests (non-Moon; the embedded-moon guard/timeout/wired tests stay in lunaris-mcp) ─

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_core::{HlcClock, Scope, StubEmbedder};
    use lunaris_test_harness::doubles::PortWithCaps;
    use lunaris_test_harness::{TestStorage, open_test_storage};

    use super::*;

    /// A live Moon that DECLARES no native queue — the one bit this module's
    /// round-trip test is about.
    ///
    /// **Re-expressed in 0.7.0.** It used to be the embedded SQLite backend,
    /// whose `queue_native == false` happened to be what the guard reads.
    /// That made the test look like a claim about `memory://` and coupled a
    /// three-line gate to a whole storage engine. The returned [`TestStorage`]
    /// owns the Moon child and must outlive the handle.
    async fn fresh_no_queue(scope_name: &str) -> (Arc<Lunaris>, Scope, TestStorage) {
        let storage = open_test_storage().await;
        let engine = Lunaris::with_parts(
            Arc::new(PortWithCaps::without_queue(storage.port())),
            Arc::new(StubEmbedder::new(768)),
            HlcClock::new(0),
        );
        let scope = Scope::new(scope_name).unwrap();
        (Arc::new(engine), scope, storage)
    }

    /// REGRESSION (codex dogfood, 2026-06-09): the response type's generated MCP
    /// `outputSchema` MUST have a root `type: "object"` — rmcp 1.7 aborts server
    /// startup otherwise. RED on a tagged enum, GREEN on the flat struct.
    #[test]
    fn response_outputschema_root_is_object() {
        let schema = schemars::schema_for!(ScratchpadConsolidateResponse);
        let v = serde_json::to_value(&schema).expect("schema serializes to JSON");
        assert_eq!(
            v.get("type").and_then(|t| t.as_str()),
            Some("object"),
            "MCP outputSchema root must be type:object; got: {v}"
        );
    }

    /// Guard 1: a backend declaring `queue_native = false` → unsupported_backend.
    #[tokio::test]
    async fn guard_queue_native_false_returns_unsupported_backend() {
        let (lunaris, scope, _storage) = fresh_no_queue("test-cons-gate").await;
        let resp = handle(&lunaris, &scope, ScratchpadConsolidateParams { namespace: None })
            .await
            .unwrap();
        assert_eq!(
            resp.status, "unsupported_backend",
            "a queue-less backend must return unsupported_backend; got: {resp:?}"
        );
    }
}
