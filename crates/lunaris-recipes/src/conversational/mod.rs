//! Conversational recipe wrappers — Phase 10 Plan 10-01 Wave A.
//!
//! Five vertical wrappers composed over [`crate::MessageStream`] +
//! [`crate::WorkingMemory`] (+ optional [`lunaris::GraphPipelineHandle`] opt-in
//! on two of them). Each wrapper has ≤ 30 LOC public surface enforced by a
//! per-file `*_public_surface_under_30_loc` compile-time test (mirrors the
//! Phase 9 PRIM-01/PRIM-04 template).
//!
//! | Wrapper | Primitives composed | Public methods |
//! | --- | --- | --- |
//! | [`ChatAgentMemory`] | `MessageStream` + `WorkingMemory` | `new` / `remember` / `recall` |
//! | [`MultiTurnConversation`] | `MessageStream` + `WorkingMemory` | `new` / `remember` / `recall` / `consolidate` |
//! | [`SlackArchive`] | `MessageStream` | `new` / `ingest_channel` / `recall` / `channel` / `user` |
//! | [`EmailThreading`] | `MessageStream` + `WorkingMemory` + `GraphPipelineHandle` | `new` / `ingest` / `thread` / `recall` / `with_graph_pipeline` |
//! | [`MeetingNotesMemory`] | `MessageStream` + `WorkingMemory` + `GraphPipelineHandle` | `new` / `note` / `recall` / `attendees` / `with_graph_pipeline` |
//!
//! Each wrapper method forwards into AT MOST 2 primitive calls — D-04 gate.
//! No new retrieval DSL. No new `Filter` variants. No new deps in Cargo.toml.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

mod chat_agent_memory;
mod email_threading;
mod meeting_notes_memory;
mod multi_turn_conversation;
mod slack_archive;

pub use chat_agent_memory::ChatAgentMemory;
pub use email_threading::EmailThreading;
pub use meeting_notes_memory::{MeetingNotesMemory, MeetingNotesQuery};
pub use multi_turn_conversation::MultiTurnConversation;
pub use slack_archive::{SlackArchive, SlackArchiveQuery};

#[cfg(test)]
mod tests {
    /// Module-level re-export smoke — matches the `conversational_mod_reexports_all_five`
    /// behaviour in Plan 10-01 Task 1. If any wrapper type fails to re-export,
    /// this line fails to compile and the test build breaks loudly.
    #[test]
    fn conversational_mod_reexports_all_five() {
        use super::{
            ChatAgentMemory, EmailThreading, MeetingNotesMemory, MultiTurnConversation,
            SlackArchive,
        };
        // Type-only smoke: reference each re-export so dead-code lints can't
        // hide a broken `pub use`. `size_of` is a no-op at runtime.
        let _ = std::mem::size_of::<ChatAgentMemory>();
        let _ = std::mem::size_of::<MultiTurnConversation>();
        let _ = std::mem::size_of::<SlackArchive>();
        let _ = std::mem::size_of::<EmailThreading>();
        let _ = std::mem::size_of::<MeetingNotesMemory>();
    }
}
