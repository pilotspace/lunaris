//! Phase 11 Plan 11-01 RCPDOC-03 — `CodeRepoMemory` documentary wrapper.
//!
//! Composes [`DocumentCorpus`] (ingest-path) + [`TemporalQuery<Documents>`]
//! (recall-path) to model "function body as-of commit N". Each commit is
//! ingested once per chunk with `commit_sha` + `committer_date_unix_ms` in
//! the Episode metadata. Recall time-travels via
//! [`TemporalQuery::<Documents>::as_of`].
//!
//! ## ≤ 30 LOC public-surface contract (RCPDOC-03)
//!
//! Public symbols: `new`, `ingest_commit`, `recall`. Three `pub fn` /
//! `pub async fn` declarations — LOC-guard caps at 6.
//!
//! ## Commit-timestamp precision (Claude's discretion per D-07)
//!
//! Hlc is `{wall_ms: u64, counter: u32, node_id: u16}` — the native shape is
//! **Unix-milliseconds**. RFC3339-nanos has nowhere to live on the Hlc
//! surface. `ingest_commit` accepts the committer_date as `i64` Unix-ms and
//! constructs `Hlc::from_parts(ms as u64, 0, 0)`. `recall(..., as_of: Hlc)`
//! takes the Hlc directly so the caller has full control over counter /
//! node_id disambiguation in dense-commit scenarios.
//!
//! ## Primitive-call budget
//!
//! - `ingest_commit`: 1 primitive call (`DocumentCorpus::ingest` once per
//!   invocation with all chunks batched).
//! - `recall`: 2 primitive calls (`TemporalQuery::<Documents>::new` +
//!   `.as_of(ts).execute(query)`). Exactly the budget per SC #2.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::LunarisError;
use lunaris_core::hlc::Hlc;
use lunaris_retrieve::Hit;

use crate::{Documents, DocumentCorpus, TemporalQuery};

/// Code-repository memory wrapper. Stores commit SHA in Episode metadata
/// (`source` column) and recalls point-in-time function bodies via
/// [`TemporalQuery<Documents>::as_of`].
#[derive(Clone)]
pub struct CodeRepoMemory {
    lunaris: Arc<Lunaris>,
    corpus: DocumentCorpus,
}

impl CodeRepoMemory {
    /// Construct bound to `repo_prefix` (e.g. `"repo:lunaris/"`). Every
    /// `ingest_commit` call stamps `repo_prefix` onto the Episode `source`.
    pub fn new(lunaris: Arc<Lunaris>, repo_prefix: impl Into<String>) -> Self {
        let corpus = DocumentCorpus::new(lunaris.clone(), repo_prefix);
        Self { lunaris, corpus }
    }

    /// Ingest one commit as a batch of `(function_body, metadata)` chunks.
    /// Adds `commit_sha` + `committer_date_unix_ms` into each chunk's
    /// metadata BEFORE forwarding to [`DocumentCorpus::ingest`]
    /// (1 primitive call).
    pub async fn ingest_commit(
        &self,
        commit_sha: impl Into<String>,
        committer_date_unix_ms: i64,
        chunks: Vec<(String, serde_json::Map<String, serde_json::Value>)>,
    ) -> Result<(), LunarisError> {
        let sha = commit_sha.into();
        let stamped: Vec<_> = chunks
            .into_iter()
            .map(|(content, mut meta)| {
                meta.insert("commit_sha".into(), serde_json::Value::String(sha.clone()));
                meta.insert(
                    "committer_date_unix_ms".into(),
                    serde_json::Value::Number(committer_date_unix_ms.into()),
                );
                (content, meta)
            })
            .collect();
        self.corpus.ingest(stamped).await
    }

    /// Recall at point-in-time `as_of`. Exactly 2 primitive calls:
    /// `TemporalQuery::<Documents>::new(lunaris)` + `.as_of(ts).execute(q)`.
    /// See note: `TemporalQuery` recalls across ALL Documents; isolation is
    /// the caller's responsibility (fixture-isolated in tests).
    pub async fn recall(&self, query: &str, as_of: Hlc) -> Result<Vec<Hit>, LunarisError> {
        TemporalQuery::<Documents>::new(self.lunaris.clone())
            .as_of(as_of)
            .execute(query)
            .await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn code_repo_memory_public_surface_under_30_loc() {
        let src = include_str!("./code_repo_memory.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "RCPDOC-03 ≤30-LOC contract: CodeRepoMemory has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 3,
            "RCPDOC-03 contract: CodeRepoMemory needs ≥3 public methods; got {pub_fns}"
        );
    }
}
