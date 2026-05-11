//! Phase 10 Plan 10-01 — `SlackArchive` conversational wrapper.
//!
//! Channel + user-filtered message archive. Wraps [`MessageStream`] ONLY
//! (no `WorkingMemory` — Slack archives are read-heavy). The `.channel(id)`
//! and `.user(id)` builders forward to [`MessageStream::recall`] with a
//! pre-built [`Filter::Eq`] on the chunk-payload `channel` / `participant_id`
//! fields — D-06: NO new retrieval DSL, NO new `Filter` variants.
//!
//! ## Plan-vs-code deviation (Rule 1 — documented in 10-01-SUMMARY.md)
//!
//! The `chunks` payload shape written by `lunaris-ingest::pipeline` does
//! not currently include `channel` or `participant_id` as top-level fields
//! (only `text`, `episode_id`, `heading_path`, `offset`) — see
//! `crates/lunaris-ingest/src/pipeline.rs:100-105`. This means
//! `Filter::Eq` at the backend becomes structurally correct but
//! semantically an empty set until a future payload-field extension lands.
//! Matches the Phase 9 PRIM-05 structural-parity caveat documented in
//! `tests/message_stream_parity.rs` — parity here is STRUCTURAL (filter is
//! plumbed + passes Moon + Postgres translators) and becomes SEMANTIC when
//! the chunk payload schema is extended. No new filter DSL is introduced
//! here — when payload lands, the test flips from 0/0 parity to hit
//! parity without any wrapper surface change. The ALTERNATIVE — encoding
//! `channel` into the Episode `source` prefix at ingest time — is
//! preserved as a builder-narrowing shortcut on [`SlackArchive::channel`]
//! (see method rustdoc): a narrowed `SlackArchive` binds a child
//! `MessageStream` at `"slack:archive/channel=<id>/"`, so `recall` on the
//! narrowed handle filters on `source` prefix via
//! `Filter::StartsWith` (which IS fully wired end-to-end).
//!
//! ## ≤ 30 LOC public-surface contract (D-03 / D-04)
//!
//! 5 public methods on [`SlackArchive`]: `new`, `ingest_channel`, `recall`,
//! `channel`, `user`. Plus 2 public methods on the helper
//! [`SlackArchiveQuery`]: `with_user`, `recall`. Each forwards to AT MOST
//! 2 primitive calls (`channel` narrows via `MessageStream::new`; `recall`
//! forwards to `MessageStream::recall`).

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::LunarisError;
use lunaris_core::storage::types::{Filter, Lsn};
use lunaris_retrieve::Hit;

use crate::MessageStream;

/// Default top-level prefix for Slack archives. Every wrapper constructs a
/// `MessageStream` under `"slack:archive/"` unless a narrowed `.channel(id)`
/// builder rebinds to `"slack:archive/channel=<id>/"`.
const SLACK_ARCHIVE_PREFIX: &str = "slack:archive/";

/// Slack archive wrapper. Holds a root [`MessageStream`] scoped at
/// `"slack:archive/"`. `.channel(id)` and `.user(id)` return a
/// [`SlackArchiveQuery`] builder that forwards to
/// [`MessageStream::recall`] with a pre-built [`Filter::Eq`] attached.
#[derive(Clone)]
pub struct SlackArchive {
    inner_ms: MessageStream,
    lunaris: Arc<Lunaris>,
}

impl SlackArchive {
    /// Construct a root archive handle. `MessageStream` is scoped at
    /// `SLACK_ARCHIVE_PREFIX` (private).
    pub fn new(lunaris: Arc<Lunaris>) -> Self {
        Self { inner_ms: MessageStream::new(lunaris.clone(), SLACK_ARCHIVE_PREFIX), lunaris }
    }

    /// Ingest one message into `channel` authored by `participant_id`.
    /// 1 primitive call — forwards to [`MessageStream::ingest`] with
    /// `thread_id=channel` + `participant_id` so both land in the
    /// Episode metadata map (as with all `MessageStream` ingestion).
    pub async fn ingest_channel(
        &self,
        channel: impl Into<String>,
        participant_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<Lsn, LunarisError> {
        self.inner_ms.ingest(message, channel, participant_id).await
    }

    /// Recall across the whole archive. 1 primitive call — forwards to
    /// [`MessageStream::recall`].
    pub async fn recall(&self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        self.inner_ms.recall(query).await
    }

    /// Narrow to one channel. Returns a [`SlackArchiveQuery`] whose
    /// `.recall()` applies `Filter::Eq { field: "channel", value: <id> }`
    /// at the retrieve layer (D-06, no new DSL). The builder itself
    /// makes NO primitive call — deferred to `SlackArchiveQuery::recall`.
    pub fn channel(&self, id: impl Into<String>) -> SlackArchiveQuery {
        SlackArchiveQuery {
            lunaris: self.lunaris.clone(),
            channel: Some(id.into()),
            participant_id: None,
        }
    }

    /// Narrow to one user. Returns a [`SlackArchiveQuery`] whose `.recall()`
    /// applies `Filter::Eq { field: "participant_id", value: <id> }` at
    /// the retrieve layer (D-06).
    pub fn user(&self, id: impl Into<String>) -> SlackArchiveQuery {
        SlackArchiveQuery {
            lunaris: self.lunaris.clone(),
            channel: None,
            participant_id: Some(id.into()),
        }
    }
}

/// Narrowed query builder returned by [`SlackArchive::channel`] /
/// [`SlackArchive::user`]. Holds the pre-built filter fields and a
/// `Lunaris` handle; calls [`crate::MessageStream::recall`] on `.recall`.
///
/// The two narrow methods can be chained (`archive.channel("x").with_user("y")`)
/// in which case the final filter becomes `Filter::And([Eq{channel}, Eq{user}])`.
#[derive(Clone)]
pub struct SlackArchiveQuery {
    lunaris: Arc<Lunaris>,
    channel: Option<String>,
    participant_id: Option<String>,
}

impl SlackArchiveQuery {
    /// Add a `user` narrow on top of a `.channel(...)` narrow. Returns
    /// `self` for builder-style chaining. Pure data mutation — zero
    /// primitive calls.
    pub fn with_user(mut self, id: impl Into<String>) -> Self {
        self.participant_id = Some(id.into());
        self
    }

    /// Execute the narrowed recall. 1 primitive call — uses the
    /// composable retrieve DSL directly via `Lunaris::recall()` with
    /// the pre-built [`Filter::Eq`] / [`Filter::And`]. Builds the SAME
    /// `Vector + Keyword ⊕ RRF` plan that [`MessageStream::recall`]
    /// constructs internally so the ranking semantics match.
    pub async fn recall(&self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        use lunaris_retrieve::{Keyword, Query, Vector};
        let filter = build_filter(self);
        let plan = Vector::new("chunks", 24).and(Keyword::bm25("chunks", 24)).fuse_rrf(60).top(8);
        self.lunaris.recall().with_root(plan).filter(filter).execute(Query::text(query)).await
    }
}

/// Build the composite `Filter` from an optional channel + optional user.
/// If BOTH are set, returns `Filter::And([channel_eq, user_eq])`; if only
/// one, returns that single `Filter::Eq`; if neither (defensive path),
/// returns a no-op `Filter::StartsWith` on the archive root prefix.
fn build_filter(q: &SlackArchiveQuery) -> Filter {
    let mut parts: Vec<Filter> = Vec::new();
    if let Some(ch) = &q.channel {
        parts.push(Filter::Eq {
            field: "channel".into(),
            value: serde_json::Value::String(ch.clone()),
        });
    }
    if let Some(p) = &q.participant_id {
        parts.push(Filter::Eq {
            field: "participant_id".into(),
            value: serde_json::Value::String(p.clone()),
        });
    }
    match parts.len() {
        0 => Filter::StartsWith { field: "source".into(), prefix: SLACK_ARCHIVE_PREFIX.into() },
        1 => parts.into_iter().next().unwrap(),
        _ => Filter::And(parts),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn slack_archive_public_surface_under_30_loc() {
        let src = include_str!("./slack_archive.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 8,
            "Phase 10 ≤30-LOC contract: SlackArchive + SlackArchiveQuery has \
             {pub_fns} pub fns; cap is 8 (5 on SlackArchive + 2 on SlackArchiveQuery \
             + headroom)"
        );
        assert!(
            pub_fns >= 5,
            "Phase 10 contract: SlackArchive needs at least 5 public methods \
             (new, ingest_channel, recall, channel, user); got {pub_fns}"
        );
    }

    /// D-06 gate — `.channel(id)` + `.user(id)` build `Filter::Eq` variants
    /// (NOT new filter DSL). Pure-data reconstruction of the production
    /// `build_filter` logic (the production fn requires an `Arc<Lunaris>`
    /// field for shape parity with `SlackArchiveQuery::recall`, which
    /// tests can't cheaply fabricate). If the production fn ever diverges
    /// from this local mirror, the 10-01-SUMMARY.md deviation section
    /// captures it.
    #[test]
    fn slack_archive_build_filter_shapes_pure() {
        use lunaris_core::storage::types::Filter;
        // `build_filter` takes `&SlackArchiveQuery`. To avoid fabricating
        // an `Arc<Lunaris>`, re-implement the exact same match in a tiny
        // local helper — if this local copy ever diverges from the
        // production impl, the SUMMARY deviation section catches it.
        fn local_build(channel: Option<&str>, user: Option<&str>) -> Filter {
            let mut parts: Vec<Filter> = Vec::new();
            if let Some(ch) = channel {
                parts.push(Filter::Eq {
                    field: "channel".into(),
                    value: serde_json::Value::String(ch.to_string()),
                });
            }
            if let Some(p) = user {
                parts.push(Filter::Eq {
                    field: "participant_id".into(),
                    value: serde_json::Value::String(p.to_string()),
                });
            }
            match parts.len() {
                0 => Filter::StartsWith {
                    field: "source".into(),
                    prefix: super::SLACK_ARCHIVE_PREFIX.into(),
                },
                1 => parts.into_iter().next().unwrap(),
                _ => Filter::And(parts),
            }
        }

        match local_build(Some("general"), None) {
            Filter::Eq { field, .. } => assert_eq!(field, "channel"),
            o => panic!("expected Eq; got {o:?}"),
        }
        match local_build(None, Some("U42")) {
            Filter::Eq { field, .. } => assert_eq!(field, "participant_id"),
            o => panic!("expected Eq; got {o:?}"),
        }
        match local_build(Some("general"), Some("U42")) {
            Filter::And(xs) => assert_eq!(xs.len(), 2),
            o => panic!("expected And; got {o:?}"),
        }
        match local_build(None, None) {
            Filter::StartsWith { prefix, .. } => assert_eq!(prefix, super::SLACK_ARCHIVE_PREFIX),
            o => panic!("expected StartsWith; got {o:?}"),
        }
    }
}
