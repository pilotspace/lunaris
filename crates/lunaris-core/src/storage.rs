//! Storage abstraction — the trait, supporting types, and capability reporting.
//!
//! See blueprint §6 for the canonical trait shape. Two deviations from the blueprint
//! are documented inline on `StoragePort`:
//!   1. Generic `Key` parameters were dropped from `scan_range` and `read_as_of` so the
//!      trait remains object-safe (`Arc<dyn StoragePort>` is a hard requirement of `STORE-08`).
//!   2. Stream items are wrapped in `Result<_, StorageError>` so backends signal mid-stream
//!      failure (network drops, timeouts) per-row instead of silently truncating.

pub mod capabilities;
pub mod port;
pub mod types;

pub use capabilities::StorageCapabilities;
pub use port::StoragePort;
pub use types::{CypherQuery, Filter, GraphResult, Key, Lsn, QueueMsg, Row, VectorHit, WriteOp};
