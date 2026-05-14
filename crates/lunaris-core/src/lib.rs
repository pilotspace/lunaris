//! lunaris-core — shared types, clock, errors, embedder trait, and storage abstraction.
#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod audit;
pub mod bitemporal;
// RFC 0007 §3.2 — per-provider circuit breaker (Closed → Open → HalfOpen).
// Consumed by the `FallbackExtractor` + `FallbackEmbedder` combinators
// in lunaris-extract / lunaris-embed; lives here because both downstream
// crates need the primitive and pulling either into the other would
// re-introduce a layering loop.
pub mod circuit_breaker;
pub mod embedder;
pub mod error;
pub mod hlc;
// Wave 2.5B: storage-agnostic primitive KV key helpers (moved from lunaris-storage-moon).
pub mod keyspace;
pub mod primitives;
// RFC 0001 — Scope newtype + ScopeError for multi-agent partition isolation.
pub mod scope;
pub mod storage;

pub use bitemporal::BiTemporal;
pub use circuit_breaker::{CircuitBreaker, CircuitConfig, CircuitState};
pub use embedder::{Embedder, NOOP_DEFAULT_DIM, NoopEmbedder, StubEmbedder};
pub use error::{
    ConsolError, ExtractError, LunarisError, RetrieveError, StorageError, ValidateError,
};
pub use hlc::{Hlc, HlcClock};
pub use primitives::{Chunk, Community, Entity, Episode, Fact, Relation};
// RFC 0001 — re-export Scope + ScopeError at the crate root so callers use
// `lunaris_core::Scope` / `lunaris::Scope` without path qualification.
pub use scope::{Scope, ScopeError};
pub use storage::{
    CypherDialect, CypherQuery, Filter, GraphResult, Key, KeywordHit, KeywordPort, Lsn, QueueMsg,
    Row, RrfFusion, StorageCapabilities, StoragePort, VectorHit, WriteOp,
};
