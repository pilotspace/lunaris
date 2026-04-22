//! lunaris-core — shared types, clock, errors, embedder trait, and storage abstraction.
#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod audit;
pub mod bitemporal;
pub mod embedder;
pub mod error;
pub mod hlc;
pub mod primitives;
pub mod storage;

pub use bitemporal::BiTemporal;
pub use embedder::{Embedder, StubEmbedder};
pub use error::{
    ConsolError, ExtractError, LunarisError, RetrieveError, StorageError, ValidateError,
};
pub use hlc::{Hlc, HlcClock};
pub use primitives::{Chunk, Community, Entity, Episode, Fact, Relation};
pub use storage::{
    CypherQuery, Filter, GraphResult, Key, KeywordHit, KeywordPort, Lsn, QueueMsg, Row, RrfFusion,
    StorageCapabilities, StoragePort, VectorHit, WriteOp,
};
