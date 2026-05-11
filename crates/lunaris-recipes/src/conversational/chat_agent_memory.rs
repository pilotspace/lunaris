//! Phase 10 Plan 10-01 — `ChatAgentMemory` conversational wrapper.
//!
//! Thin composition over [`MessageStream`] (turn ingestion + recall) and
//! [`WorkingMemory`] (per-user scratchpad) scoped via `"chat:<user_id>/"`
//! so a Phase 9.1 `WorkingMemory::consolidate()` pass (invoked by the
//! parent `MultiTurnConversation` wrapper) promotes per-user scratchpad
//! notes without leaking into other users' scopes (closes Phase 10 risk
//! register row #5).
//!
//! ## ≤ 30 LOC public-surface contract (D-03 / D-04)
//!
//! 3 public methods: `new`, `remember`, `recall`. Each forwards to AT
//! MOST 1 primitive call. LOC-guard test at the bottom caps the count
//! at 6 (room for 3 additive future methods within the same contract).

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::LunarisError;
use lunaris_core::storage::types::Lsn;
use lunaris_retrieve::Hit;

use crate::{MessageStream, WorkingMemory};

/// Conversational wrapper exposing a `remember` / `recall` pair over a
/// per-user `MessageStream + WorkingMemory` pair. The wrapper holds both
/// primitives so future additive surface (e.g., `::scratchpad()` accessor)
/// can reuse the already-scoped handles without re-constructing them.
#[derive(Clone)]
pub struct ChatAgentMemory {
    inner_ms: MessageStream,
    // Held for D-05 scope-prefix discipline and for future additive surface
    // (scratchpad accessor). The current public surface does not forward
    // into it — `MultiTurnConversation` owns the `.consolidate()` method.
    #[allow(dead_code)]
    inner_wm: WorkingMemory,
}

impl ChatAgentMemory {
    /// Construct a `ChatAgentMemory` bound to `user_id`. Both inner
    /// primitives receive the SAME `"chat:<user_id>/"` scope prefix —
    /// D-05 invariant that prevents cross-user consolidator leaks in
    /// Phase 10 risk register row #5.
    pub fn new(lunaris: Arc<Lunaris>, user_id: &str) -> Self {
        let scope = format!("chat:{}/", user_id);
        Self {
            inner_ms: MessageStream::new(lunaris.clone(), scope.clone()),
            inner_wm: WorkingMemory::new(lunaris, scope),
        }
    }

    /// Record a single conversational turn. Forwards to
    /// [`MessageStream::ingest`] (1 primitive call).
    ///
    /// `thread_id="default"` + `participant_id="user"` — chat sessions are a
    /// flat stream in this wrapper. Consumers wanting per-thread partitioning
    /// compose [`MultiTurnConversation`](super::MultiTurnConversation) instead.
    pub async fn remember(&self, turn: impl Into<String>) -> Result<Lsn, LunarisError> {
        self.inner_ms.ingest(turn, "default", "user").await
    }

    /// Recall turns matching `query`. Forwards to [`MessageStream::recall`]
    /// (1 primitive call). Ordering is the ACT-R recency-weighted blend
    /// inherited from the underlying primitive — see
    /// [`crate::MessageStream::recall`] rustdoc.
    pub async fn recall(&self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        self.inner_ms.recall(query).await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn chat_agent_memory_public_surface_under_30_loc() {
        let src = include_str!("./chat_agent_memory.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "Phase 10 ≤30-LOC contract: ChatAgentMemory has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 3,
            "Phase 10 contract: ChatAgentMemory needs at least 3 public methods \
             (new, remember, recall); got {pub_fns}"
        );
    }
}
