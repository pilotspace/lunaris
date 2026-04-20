//! Plan 04-01 Task 1 STUB (B-7 fix). Full body lands in Task 3 — this stub
//! exposes the public constants + `run_verify_worker` signature so `pub mod
//! worker;` in lib.rs compiles after Task 1 alone.
//!
//! The subscribe loop, `tokio::sync::Notify` shutdown drain (D-07), MVCC
//! supersede atomic_write (D-11), and audit publish (D-22) are wired in
//! Task 3 of this plan. The `todo!()` body here is deliberate — no caller
//! invokes `run_verify_worker` until Plan 04-04 Task 2 wires it into
//! `VerifierPipelineHandle::enable()`.

use std::sync::Arc;

use lunaris_core::{LunarisError, StoragePort};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::Verifier;

/// Consumer group name (D-06). Versioned so a v1 message-schema bump can use
/// a fresh consumer group without colliding with running v0 workers.
pub const VERIFY_CONSUMER_GROUP: &str = "lunaris-verify-v0";

/// Verify queue topic (matches Plan 03-03 `ingest.rs::VERIFY_QUEUE_TOPIC`).
pub const VERIFY_TOPIC: &str = "__lunaris_verify__";

/// STUB — Task 3 replaces this body with the real subscribe loop +
/// `apply_supersede` + audit emit. Returning `todo!()` is acceptable at the
/// stub stage because no caller invokes this fn until Plan 04-04 wires it
/// into `VerifierPipelineHandle::enable()`.
pub async fn run_verify_worker(
    _storage: Arc<dyn StoragePort>,
    _verifier: Arc<dyn Verifier>,
    _shutdown: Arc<Notify>,
) -> Result<JoinHandle<()>, LunarisError> {
    todo!("Task 3 ships the full worker body")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumer_group_and_topic_are_versioned() {
        assert_eq!(VERIFY_CONSUMER_GROUP, "lunaris-verify-v0");
        assert_eq!(VERIFY_TOPIC, "__lunaris_verify__");
    }
}
