//! Session-handover consolidation — the whole-scope ACT-R drain triggered when
//! the mcp caller detects a session change (scratchpad-proxiable task).
//!
//! Moved from `lunaris-mcp`'s `run_handover_consolidate`. It runs on the warm
//! engine that owns the pad (contextd over the socket, or the mcp direct-open
//! fallback) rather than a second local engine.
//!
//! Same three guards as the tool handler, around the WHOLE-SCOPE drain
//! (`consolidate_unfiltered` — a prefix filter would drop drained events
//! belonging to other namespaces).
//!
//! **Warn-and-continue by design**: handover failures and guard refusals must
//! NEVER error the tool call that triggered them. This handler is INFALLIBLE
//! (always `Ok` at dispatch): the returned `status` is advisory only — the mcp
//! caller logs it and carries the old pad forward, retrying at the next switch.

use std::sync::Arc;

use lunaris::{Lunaris, WorkingMemory};
use lunaris_core::Scope;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use crate::scratchpad_consolidate::CONSOLIDATE_TOOL_TIMEOUT;

/// Advisory outcome of a session-handover drain.
///
/// Flat struct (MCP outputSchema root must be `type:object`). `status` is one
/// of `ok` | `skipped_no_queue` | `skipped_worker_conflict` | `timeout` |
/// `error`. All are non-fatal — the caller treats every value as best-effort.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HandoverResponse {
    /// Advisory outcome discriminator.
    pub status: String,
    /// Facts promoted (0 unless `status == "ok"`).
    pub promotions: usize,
    /// Facts archived (0 unless `status == "ok"`).
    pub archives: usize,
}

impl HandoverResponse {
    fn of(status: &str) -> Self {
        Self { status: status.into(), promotions: 0, archives: 0 }
    }
}

/// Run the whole-scope handover consolidate. Infallible: every path returns a
/// `HandoverResponse` and logs; the caller never sees an error from a handover.
pub async fn handle(lunaris: &Arc<Lunaris>, scope: &Scope) -> HandoverResponse {
    // Guard 1: queue_native gate.
    if !lunaris.storage().capabilities().queue_native {
        tracing::warn!(
            scope = scope.as_str(),
            "session handover: backend has no native queue — skipping consolidate, \
             previous pad carries forward",
        );
        return HandoverResponse::of("skipped_no_queue");
    }
    // Guard 2: background worker refusal (single __mq_consumers group).
    if lunaris.consolidator_pipeline().is_enabled() {
        tracing::warn!(
            scope = scope.as_str(),
            "session handover: background consolidation worker is live — skipping \
             consolidate to avoid double-consume, previous pad carries forward",
        );
        return HandoverResponse::of("skipped_worker_conflict");
    }
    // The namespace on this WorkingMemory is irrelevant to the unfiltered drain;
    // pass the legacy default for the audit trail.
    let wm = WorkingMemory::new(lunaris.clone(), scope.clone(), "scratchpad/".to_owned());
    // Guard 3: hard timeout.
    match timeout(CONSOLIDATE_TOOL_TIMEOUT, wm.consolidate_unfiltered()).await {
        Err(_elapsed) => {
            tracing::warn!(
                scope = scope.as_str(),
                "session handover: consolidate exceeded the hard timeout — partial \
                 progress possible, retrying at the next switch",
            );
            HandoverResponse::of("timeout")
        }
        Ok(Err(e)) => {
            tracing::warn!(
                scope = scope.as_str(), err = %e,
                "session handover: consolidate failed — previous pad carries forward",
            );
            HandoverResponse::of("error")
        }
        Ok(Ok(report)) => {
            tracing::info!(
                scope = scope.as_str(),
                promotions = report.promotions.len(),
                archives = report.archives.len(),
                "session handover: previous session's pending events consolidated",
            );
            HandoverResponse {
                status: "ok".into(),
                promotions: report.promotions.len(),
                archives: report.archives.len(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lunaris_core::{HlcClock, Scope, StubEmbedder};
    use lunaris_test_harness::doubles::PortWithCaps;
    use lunaris_test_harness::open_test_storage;

    use super::*;

    /// Guard 1 on a backend that declares no native queue: infallible skip,
    /// never errors.
    ///
    /// **Re-expressed in 0.7.0.** This used to open the embedded SQLite
    /// backend, whose `queue_native == false` was doing the work. That
    /// coupled a three-line guard to a whole storage engine — and made the
    /// test read as a claim about `memory://` when it is a claim about one
    /// bool. It now runs against a real Moon with that single bit cleared
    /// ([`PortWithCaps::without_queue`]), so the guard is exercised directly
    /// and every other call still reaches a live store.
    #[tokio::test]
    async fn handover_on_no_queue_backend_skips() {
        // Bind the fixture: it owns the Moon child for the test's lifetime.
        let storage = open_test_storage().await;
        let lunaris = Arc::new(Lunaris::with_parts(
            Arc::new(PortWithCaps::without_queue(storage.port())),
            Arc::new(StubEmbedder::new(768)),
            HlcClock::new(0),
        ));
        let scope = Scope::new("test-handover-noqueue").unwrap();
        let resp = handle(&lunaris, &scope).await;
        assert_eq!(
            resp.status, "skipped_no_queue",
            "a queue-less backend must skip handover; got: {resp:?}"
        );
    }
}
