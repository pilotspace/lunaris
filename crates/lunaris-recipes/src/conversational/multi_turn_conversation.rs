//! Phase 10 Plan 10-01 — `MultiTurnConversation` conversational wrapper.
//!
//! Adds a per-user scoped `.consolidate()` cross-session promotion pass on
//! top of the [`ChatAgentMemory`](crate::conversational::ChatAgentMemory)
//! shape. Uses `scope_prefix = "chat:<user_id>/"` on BOTH the inner
//! [`MessageStream`] and [`WorkingMemory`], so the Phase 9.1
//! `Consolidator::consolidate_scoped(Some(prefix))` default-method filter
//! (wired at `WorkingMemory::consolidate`) rejects any event whose
//! `source` does not start with `chat:<user_id>/` — closing Phase 10
//! risk register row #5 (cross-user consolidator leak).
//!
//! ## ≤ 30 LOC public-surface contract (D-03 / D-04)
//!
//! 4 public methods: `new`, `remember`, `recall`, `consolidate`. Each
//! forwards to AT MOST 1 primitive call.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_consolidate::ConsolidationReport;
use lunaris_core::storage::types::Lsn;
use lunaris_core::LunarisError;
use lunaris_retrieve::Hit;

use crate::{MessageStream, WorkingMemory};

/// Cross-session conversational wrapper. Shares the same `"chat:<user_id>/"`
/// scope across message ingestion AND the scratchpad promotion hook, so
/// `::consolidate()` runs with scope isolation turned on by construction.
#[derive(Clone)]
pub struct MultiTurnConversation {
    inner_ms: MessageStream,
    inner_wm: WorkingMemory,
}

impl MultiTurnConversation {
    /// Construct bound to `user_id`. Scope prefix `"chat:<user_id>/"` is
    /// captured on BOTH inner primitives — the write path
    /// ([`MessageStream::ingest`]) and the promotion filter
    /// ([`WorkingMemory::consolidate`]) share the same prefix so the
    /// per-user scope invariant is enforced at both ends of the pipeline.
    pub fn new(lunaris: Arc<Lunaris>, user_id: &str) -> Self {
        let scope = format!("chat:{}/", user_id);
        Self {
            inner_ms: MessageStream::new(lunaris.clone(), scope.clone()),
            inner_wm: WorkingMemory::new(lunaris, scope),
        }
    }

    /// Record a turn for `thread_id` (session id). 1 primitive call —
    /// forwards to [`MessageStream::ingest`] with `participant_id="user"`.
    pub async fn remember(
        &self,
        turn: impl Into<String>,
        thread_id: impl Into<String>,
    ) -> Result<Lsn, LunarisError> {
        self.inner_ms.ingest(turn, thread_id, "user").await
    }

    /// Recall turns matching `query` across all sessions. 1 primitive
    /// call — forwards to [`MessageStream::recall`].
    pub async fn recall(&self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        self.inner_ms.recall(query).await
    }

    /// Run one scope-filtered consolidation pass. 1 primitive call —
    /// forwards to [`WorkingMemory::consolidate`]. Scope prefix was
    /// captured at `new`; non-`chat:<user_id>/` events are rejected by
    /// `Consolidator::consolidate_scoped` (Phase 9.1 Plan 09.1-01).
    pub async fn consolidate(&self) -> Result<ConsolidationReport, LunarisError> {
        self.inner_wm.consolidate().await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn multi_turn_conversation_public_surface_under_30_loc() {
        let src = include_str!("./multi_turn_conversation.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "Phase 10 ≤30-LOC contract: MultiTurnConversation has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 4,
            "Phase 10 contract: MultiTurnConversation needs at least 4 public methods \
             (new, remember, recall, consolidate); got {pub_fns}"
        );
    }
}
