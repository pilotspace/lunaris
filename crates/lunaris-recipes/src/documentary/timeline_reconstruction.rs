//! Phase 11 Plan 11-01 RCPDOC-04 — `TimelineReconstruction` documentary
//! wrapper. Composes [`DocumentCorpus`] (ingest) + [`TemporalQuery<Documents>`]
//! with `.between(lo, hi)` + `.as_of(ts)` (recall).
//!
//! ## ≤ 30 LOC public-surface contract (RCPDOC-04)
//!
//! Public symbols: `new`, `ingest`, `between`, `as_of`. Four `pub fn` /
//! `pub async fn` declarations — LOC-guard caps at 6. Per ROADMAP risk row
//! #3 this wrapper is deliberately thin (< 10 LOC is acceptable); its value
//! is discoverability as a named recipe, not code volume.
//!
//! ## `.between` boundary semantics (flagged for 11-03)
//!
//! Phase 9.1 backend rendering emits
//! `valid_from >= lo AND valid_from < hi` (Postgres) / `@valid_time:[lo hi]`
//! (Moon) — **lower-bound inclusive, upper-bound exclusive**. Callers that
//! want "days X..=Y inclusive" must pass `hi = Y + 1_day`. This carries
//! straight into the 11-03 Py/TS parity tests; document there too.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::LunarisError;
use lunaris_core::hlc::Hlc;
use lunaris_retrieve::Hit;

use crate::{DocumentCorpus, Documents, TemporalQuery};

/// Timeline-reconstruction wrapper. Two-call composition of
/// DocumentCorpus + TemporalQuery<Documents>.
#[derive(Clone)]
pub struct TimelineReconstruction {
    lunaris: Arc<Lunaris>,
    corpus: DocumentCorpus,
}

impl TimelineReconstruction {
    /// Construct bound to `source_prefix` (e.g. `"timeline:events/"`).
    pub fn new(lunaris: Arc<Lunaris>, source_prefix: impl Into<String>) -> Self {
        let corpus = DocumentCorpus::new(lunaris.clone(), source_prefix);
        Self { lunaris, corpus }
    }

    /// Ingest timeline events as chunked `(content, metadata)` pairs.
    /// Forwards to [`DocumentCorpus::ingest`] (1 primitive call).
    pub async fn ingest(
        &self,
        events: Vec<(String, serde_json::Map<String, serde_json::Value>)>,
    ) -> Result<(), LunarisError> {
        self.corpus.ingest(events).await
    }

    /// Recall all events in `[lo, hi)` (lower inclusive, upper exclusive).
    /// 2 primitive calls: `TemporalQuery::<Documents>::new` + `.between().execute()`.
    pub async fn between(&self, query: &str, lo: Hlc, hi: Hlc) -> Result<Vec<Hit>, LunarisError> {
        TemporalQuery::<Documents>::new(self.lunaris.clone()).between(lo, hi).execute(query).await
    }

    /// Recall the snapshot at `ts`. 2 primitive calls:
    /// `TemporalQuery::<Documents>::new` + `.as_of(ts).execute()`.
    pub async fn as_of(&self, query: &str, ts: Hlc) -> Result<Vec<Hit>, LunarisError> {
        TemporalQuery::<Documents>::new(self.lunaris.clone()).as_of(ts).execute(query).await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn timeline_reconstruction_public_surface_under_30_loc() {
        let src = include_str!("./timeline_reconstruction.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "RCPDOC-04 ≤30-LOC contract: TimelineReconstruction has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 2,
            "RCPDOC-04 contract: TimelineReconstruction needs ≥2 public methods; got {pub_fns}"
        );
    }
}
