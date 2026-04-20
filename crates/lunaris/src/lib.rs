//! lunaris — umbrella crate. Re-exports `lunaris_core` types and exposes the
//! `open(url)` URL-scheme dispatcher.
#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod open;

pub use lunaris_core::*;
pub use open::open;

// Re-export backend concrete types for callers who want to construct directly
// (bypassing URL routing — needed by the conformance harness in Phase 5).
pub use lunaris_storage_moon::MoonStorage;
pub use lunaris_storage_postgres::PostgresStorage;
