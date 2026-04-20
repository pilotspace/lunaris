//! `Lunaris::ingest` — the public Phase 2 entry point. Delegates to
//! [`lunaris_ingest::ingest_episode`] with the handle's storage + embedder
//! + clock. Closes INGEST-03.

use lunaris_core::{Episode, Lsn, LunarisError};

use crate::handle::Lunaris;

impl Lunaris {
    /// Ingest one [`Episode`] through the Phase 2 hot path.
    ///
    /// Pipeline:
    ///  1. Markdown chunker (500 token target / 100 overlap, heading_path preserved)
    ///  2. Batched embedder driver (32 chunks per call; per-chunk fallback on batch error)
    ///  3. Single [`StoragePort::atomic_write`](lunaris_core::StoragePort::atomic_write)
    ///     containing the Episode + all Chunks (INGEST-04 — all-or-nothing).
    ///
    /// Returns the [`Lsn`] at which the writes became visible.
    pub async fn ingest(&self, episode: Episode) -> Result<Lsn, LunarisError> {
        lunaris_ingest::ingest_episode(
            self.storage.as_ref(),
            self.embedder.as_ref(),
            &self.clock,
            episode,
        )
        .await
    }
}
