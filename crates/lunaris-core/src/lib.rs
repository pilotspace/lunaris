//! lunaris-core — shared types, clock, errors, and embedder trait.
#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod bitemporal;
pub mod embedder;
pub mod error;
pub mod hlc;

pub use bitemporal::BiTemporal;
pub use embedder::{Embedder, StubEmbedder};
pub use error::{
    ConsolError, ExtractError, LunarisError, RetrieveError, StorageError, ValidateError,
};
pub use hlc::{Hlc, HlcClock};
