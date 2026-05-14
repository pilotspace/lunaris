//! Compatibility shim — N-03 cutover.
//!
//! `NoopEmbedder` + `NOOP_DEFAULT_DIM` were moved into `lunaris-core` so the
//! noop path survives the `lunaris-embed` crate's deletion. This module
//! re-exports them so v0.3 callers using `lunaris_embed::NoopEmbedder` keep
//! compiling until they migrate to `lunaris_core::NoopEmbedder`. The whole
//! `lunaris-embed` crate is removed in the next commit.

pub use lunaris_core::{NOOP_DEFAULT_DIM, NoopEmbedder};
