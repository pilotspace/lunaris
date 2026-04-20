//! lunaris — umbrella crate. Re-exports `lunaris_core` types, exposes the
//! `open(url)` URL-scheme dispatcher AND the higher-level `Lunaris` handle
//! that drives the Phase 2 ingest hot path.
//!
//! ## Two construction paths
//!
//! - [`open()`](crate::open::open) — returns `Arc<dyn StoragePort>` for
//!   callers that just want raw storage access (Plan 5 conformance harness,
//!   low-level tests).
//! - [`Lunaris::open`](crate::handle::Lunaris::open) — returns a high-level
//!   handle wired with a default [`Embedder`] + [`HlcClock`] so callers can
//!   call `lunaris.ingest(episode).await?` without manually plumbing the
//!   Phase 2 pipeline. This is what Helios uses.
#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod handle;
pub mod ingest;
pub mod open;

pub use handle::Lunaris;
pub use lunaris_core::*;
pub use open::open;

// Re-export backend concrete types for callers who want to construct directly
// (bypassing URL routing — needed by the conformance harness in Phase 5).
pub use lunaris_storage_moon::MoonStorage;
pub use lunaris_storage_postgres::PostgresStorage;
