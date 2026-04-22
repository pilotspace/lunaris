//! Phase 10 Plan 10-01 — `MeetingNotesMemory` conversational wrapper.
//!
//! Meeting-heading-scoped notes archive with an opt-in graph-pipeline
//! builder. Composes [`MessageStream`] + [`WorkingMemory`] under a
//! `"meeting:notes/"` prefix; `.attendees(list)` returns a narrowed query
//! with `Filter::Eq { field: "participant_id", .. }` AND semantics;
//! `.with_graph_pipeline(true)` flips the v0.1.0 graph handle just like
//! [`EmailThreading`] (SC #3).
//!
//! ## ≤ 30 LOC public-surface contract (D-03 / D-04)
//!
//! 5 public methods: `new`, `note`, `recall`, `attendees`, `with_graph_pipeline`.
//! Each forwards to AT MOST 1 primitive call.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::storage::types::{Filter, Lsn};
use lunaris_core::LunarisError;
use lunaris_retrieve::Hit;

use crate::{MessageStream, WorkingMemory};

/// Default top-level prefix for meeting-notes archives.
const MEETING_PREFIX: &str = "meeting:notes/";

/// Meeting-notes wrapper. Stores headings as `thread_id` and bodies as
/// message content via [`MessageStream::ingest`]. `.attendees()` returns
/// a narrowed query with a `Filter::And` of per-attendee `Filter::Eq`.
#[derive(Clone)]
pub struct MeetingNotesMemory {
    inner_ms: MessageStream,
    // D-05: WorkingMemory at the same scope for future additive surface.
    #[allow(dead_code)]
    inner_wm: WorkingMemory,
    lunaris: Arc<Lunaris>,
}

impl MeetingNotesMemory {
    /// Construct bound to [`MEETING_PREFIX`].
    pub fn new(lunaris: Arc<Lunaris>) -> Self {
        Self {
            inner_ms: MessageStream::new(lunaris.clone(), MEETING_PREFIX),
            inner_wm: WorkingMemory::new(lunaris.clone(), MEETING_PREFIX),
            lunaris,
        }
    }

    /// Record one note under `heading` (thread_id) with `body` content.
    /// Participant defaults to `"scribe"` — wrappers wanting per-attendee
    /// attribution should use [`crate::MessageStream::ingest`] directly.
    /// 1 primitive call.
    pub async fn note(
        &self,
        heading: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<Lsn, LunarisError> {
        self.inner_ms.ingest(body, heading, "scribe").await
    }

    /// Recall across the meeting corpus. 1 primitive call — forwards to
    /// [`MessageStream::recall`].
    pub async fn recall(&self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        self.inner_ms.recall(query).await
    }

    /// Narrow to notes attributed to `attendees`. Returns a
    /// [`MeetingNotesQuery`] whose `.recall()` emits a `Filter::And` of
    /// per-attendee `Filter::Eq { field: "participant_id", .. }` — D-06:
    /// NO new filter DSL, NO new `Filter` variants. 0 primitive calls
    /// (deferred).
    // Plan 11-02b Rule 1 widen: accept `Vec<String>` instead of `&[&str]`
    // so the codegen `StrList` (Vec<String>) allow-list entry can flow
    // cleanly. ≤3 LOC signature change; no new filter DSL; production
    // semantics unchanged. All existing call sites pass owned `Vec<String>`
    // or `vec!["x".into(), ...]` literals which transparently coerce.
    pub fn attendees(&self, attendees: Vec<String>) -> MeetingNotesQuery {
        MeetingNotesQuery {
            lunaris: self.lunaris.clone(),
            participants: attendees,
        }
    }

    /// Opt-in the v0.1.0 graph pipeline (D-07). Blueprint §5.2
    /// graph-default-off preserved when `enable=false`.
    pub fn with_graph_pipeline(self, enable: bool) -> Self {
        if enable {
            self.lunaris.graph_pipeline().enable();
        } else {
            self.lunaris.graph_pipeline().disable();
        }
        self
    }
}

/// Narrowed query builder returned by [`MeetingNotesMemory::attendees`].
/// `.recall` executes through the composable retrieve DSL directly (same
/// pattern as [`crate::conversational::SlackArchiveQuery`]) — 1 primitive
/// call, no new DSL.
#[derive(Clone)]
pub struct MeetingNotesQuery {
    lunaris: Arc<Lunaris>,
    participants: Vec<String>,
}

impl MeetingNotesQuery {
    /// Execute the narrowed recall. Combines per-attendee
    /// `Filter::Eq { field: "participant_id", value: <id> }` into
    /// `Filter::And` (all attendees must be present) when multiple,
    /// else a single `Eq`.
    pub async fn recall(&self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        use lunaris_retrieve::{Keyword, Query, Vector};
        let filter = build_attendees_filter(&self.participants);
        let plan = Vector::new("chunks", 24)
            .and(Keyword::bm25("chunks", 24))
            .fuse_rrf(60)
            .top(8);
        self.lunaris
            .recall()
            .with_root(plan)
            .filter(filter)
            .execute(Query::text(query))
            .await
    }
}

/// Pure helper — build the filter tree from an attendee list.
fn build_attendees_filter(participants: &[String]) -> Filter {
    let parts: Vec<Filter> = participants
        .iter()
        .map(|p| Filter::Eq {
            field: "participant_id".into(),
            value: serde_json::Value::String(p.clone()),
        })
        .collect();
    match parts.len() {
        0 => Filter::StartsWith {
            field: "source".into(),
            prefix: MEETING_PREFIX.into(),
        },
        1 => parts.into_iter().next().unwrap(),
        _ => Filter::And(parts),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn meeting_notes_memory_public_surface_under_30_loc() {
        let src = include_str!("./meeting_notes_memory.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 8,
            "Phase 10 ≤30-LOC contract: MeetingNotesMemory + MeetingNotesQuery has \
             {pub_fns} pub fns; cap is 8"
        );
        assert!(
            pub_fns >= 5,
            "Phase 10 contract: MeetingNotesMemory needs at least 5 public methods \
             (new, note, recall, attendees, with_graph_pipeline); got {pub_fns}"
        );
    }

    /// Pure-data gate on [`build_attendees_filter`] — locks the field
    /// name `participant_id` and the empty / single / multi shapes.
    #[test]
    fn meeting_notes_attendees_filter_shapes() {
        use super::{build_attendees_filter, MEETING_PREFIX};
        use lunaris_core::storage::types::Filter;
        match build_attendees_filter(&[]) {
            Filter::StartsWith { prefix, .. } => assert_eq!(prefix, MEETING_PREFIX),
            o => panic!("expected StartsWith; got {o:?}"),
        }
        match build_attendees_filter(&["alice".to_string()]) {
            Filter::Eq { field, .. } => assert_eq!(field, "participant_id"),
            o => panic!("expected Eq; got {o:?}"),
        }
        match build_attendees_filter(&["alice".to_string(), "bob".to_string()]) {
            Filter::And(xs) => assert_eq!(xs.len(), 2),
            o => panic!("expected And; got {o:?}"),
        }
    }
}
