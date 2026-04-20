//! Plan 04-02 D-04..D-07 + D-16..D-17: in-process tokio worker that consumes
//! `__lunaris_consolidate__`, debounces events, and runs activation passes.
//!
//! B-7 forward-compat stub: Task 1 commits this file with the public
//! [`run_consolidate_worker`] signature + versioned constants so `cargo check
//! -p lunaris-consolidate` passes after Task 1 alone. Task 3 replaces the
//! body with the real subscribe loop + debounce buffer + per-promotion /
//! per-archive audit emit (B-1 — D-22 verbatim).

use std::sync::Arc;

use lunaris_core::{LunarisError, StoragePort};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::Consolidator;

/// Consumer group name (D-06). Versioned so a v1 message-schema bump can use
/// a fresh consumer group without colliding with running v0 workers.
pub const CONSOLIDATE_CONSUMER_GROUP: &str = "lunaris-consolidate-v0";

/// Consolidate queue topic.
pub const CONSOLIDATE_TOPIC: &str = "__lunaris_consolidate__";

/// Spawn the in-process consolidator worker (Task 3 body).
///
/// Task 3 wires the subscribe loop, debounce buffer (D-17
/// `LUNARIS_CONSOLIDATE_DEBOUNCE_MS`, default 60 s), `tokio::sync::Notify`
/// shutdown, and per-event audit emit (B-1 — one
/// `AuditEvent::ConsolidatorPromotion` per promotion + one
/// `AuditEvent::ConsolidatorArchive` per archive). Preserves the
/// single-`atomic_write` invariant at the caller layer — v0 body does not
/// itself issue atomic_write since the NoopConsolidator short-circuit is the
/// default path.
pub async fn run_consolidate_worker(
    _storage: Arc<dyn StoragePort>,
    _consolidator: Arc<dyn Consolidator>,
    _shutdown: Arc<Notify>,
) -> Result<JoinHandle<()>, LunarisError> {
    unimplemented!("Task 3 replaces this stub with the subscribe loop + debounce buffer + per-event audit emit (B-1)")
}
