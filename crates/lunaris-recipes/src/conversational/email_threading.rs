//! Phase 10 Plan 10-01 — `EmailThreading` conversational wrapper.
//!
//! Thread-scoped email archive with an opt-in graph-pipeline builder.
//! Composes [`MessageStream`] + [`WorkingMemory`] under a
//! `"email:thread/<root_id>/"` scope prefix; `.with_graph_pipeline(true)`
//! calls [`lunaris::GraphPipelineHandle::enable`] on the handle (SC #3,
//! blueprint §5.2 graph-default-off preserved — user must opt in).
//!
//! ## ≤ 30 LOC public-surface contract (D-03 / D-04)
//!
//! 5 public methods: `new`, `ingest`, `thread`, `recall`, `with_graph_pipeline`.
//! Each forwards to AT MOST 1 primitive call (2 for `with_graph_pipeline`
//! which flips the handle AND returns `self`, but the handle toggle is a
//! `GraphPipelineHandle::enable()` call that is NOT a retrieval primitive
//! — D-07).

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::LunarisError;
use lunaris_core::storage::types::Lsn;
use lunaris_retrieve::Hit;

use crate::{MessageStream, WorkingMemory};

/// Default top-level prefix for email archives.
const EMAIL_PREFIX: &str = "email:thread/";

/// Thread-scoped email wrapper. Holds a root [`MessageStream`] scoped at
/// `EMAIL_PREFIX` (private) and an `Arc<Lunaris>` re-used by `.thread()` to narrow
/// and by `.with_graph_pipeline()` to toggle the graph handle.
#[derive(Clone)]
pub struct EmailThreading {
    inner_ms: MessageStream,
    // D-05 discipline: WorkingMemory at the same scope so future additive
    // surface (e.g., per-thread scratchpad for draft replies) can land
    // without re-constructing the primitive. Not currently forwarded to
    // by any public method.
    #[allow(dead_code)]
    inner_wm: WorkingMemory,
    lunaris: Arc<Lunaris>,
}

impl EmailThreading {
    /// Construct a new email-archive handle rooted at `EMAIL_PREFIX` (private).
    pub fn new(lunaris: Arc<Lunaris>) -> Self {
        Self {
            inner_ms: MessageStream::new(lunaris.clone(), EMAIL_PREFIX),
            inner_wm: WorkingMemory::new(lunaris.clone(), EMAIL_PREFIX),
            lunaris,
        }
    }

    /// Ingest one email into `root_id`'s thread, authored by `from`.
    /// 1 primitive call — forwards to [`MessageStream::ingest`].
    pub async fn ingest(
        &self,
        root_id: impl Into<String>,
        from: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<Lsn, LunarisError> {
        self.inner_ms.ingest(body, root_id, from).await
    }

    /// Narrow to one thread `root_id`. Returns a new [`EmailThreading`]
    /// whose inner [`MessageStream`] is scoped at
    /// `"email:thread/<root_id>/"` — the `Filter::StartsWith` on `source`
    /// does the thread-narrowing work via the primitive's existing
    /// filter, NOT a new DSL. 0 primitive calls (pure construction).
    pub fn thread(&self, root_id: impl Into<String>) -> Self {
        let scope = format!("{}{}/", EMAIL_PREFIX, root_id.into());
        Self {
            inner_ms: MessageStream::new(self.lunaris.clone(), scope.clone()),
            inner_wm: WorkingMemory::new(self.lunaris.clone(), scope),
            lunaris: self.lunaris.clone(),
        }
    }

    /// Recall across the current scope. 1 primitive call — forwards to
    /// [`MessageStream::recall`].
    pub async fn recall(&self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        self.inner_ms.recall(query).await
    }

    /// Opt-in the v0.1.0 graph pipeline on the shared [`Lunaris`] handle.
    /// Idempotent; does not consume retrieval primitives. Returns `self`
    /// for builder-style chaining. Toggle surface is
    /// [`lunaris::GraphPipelineHandle::enable`] / `disable` — NO new
    /// retrieval DSL (D-07, blueprint §5.2 graph-default-off preserved
    /// when `enable=false`).
    pub fn with_graph_pipeline(self, enable: bool) -> Self {
        if enable {
            self.lunaris.graph_pipeline().enable();
        } else {
            self.lunaris.graph_pipeline().disable();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn email_threading_public_surface_under_30_loc() {
        let src = include_str!("./email_threading.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "Phase 10 ≤30-LOC contract: EmailThreading has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 5,
            "Phase 10 contract: EmailThreading needs at least 5 public methods \
             (new, ingest, thread, recall, with_graph_pipeline); got {pub_fns}"
        );
    }
}
