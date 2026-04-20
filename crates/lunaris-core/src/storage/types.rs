//! Supporting types referenced by the `StoragePort` trait.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::bitemporal::BiTemporal;
use crate::error::StorageError;
use crate::hlc::Hlc;

// ----- Lsn -----

/// Log sequence number returned by `atomic_write`.
///
/// Backends derive this from the HLC issued at commit time. Higher Lsn = later write.
/// Lsns are totally ordered by `(wall_ms, counter)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Lsn {
    pub wall_ms: u64,
    pub counter: u32,
}

impl Lsn {
    pub const ZERO: Lsn = Lsn { wall_ms: 0, counter: 0 };

    pub fn from_hlc(h: Hlc) -> Self {
        Self { wall_ms: h.wall_ms, counter: h.counter }
    }
}

// ----- Key trait -----

/// User-defined typed key. Implementors define how to serialize the key to bytes
/// for `KvPut` / `KvDelete` write ops; trait is object-safe-by-design (no generic methods).
///
/// The `StoragePort` trait deliberately does NOT take `K: Key` on `scan_range` /
/// `read_as_of` — generic methods break `Arc<dyn StoragePort>` (object safety). Callers
/// with typed keys call `.as_bytes()` at the call site and pass `&[u8]`.
pub trait Key: Send + Sync {
    fn as_bytes(&self) -> Vec<u8>;
    fn from_bytes(b: &[u8]) -> Result<Self, StorageError>
    where
        Self: Sized;
}

// ----- WriteOp -----

/// One operation in an `atomic_write` batch.
///
/// All variants must commit together or all roll back. Backends translate each variant
/// to their native command (e.g., `HSET`, `FT.SUGADD`, `GRAPH.QUERY MERGE` on Moon;
/// `INSERT`, `pgvector` upsert, AGE Cypher `MERGE` on Postgres).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WriteOp {
    /// KV put at this key/value pair.
    KvPut { key: Vec<u8>, value: Vec<u8> },
    /// KV tombstone (logical delete; physical removal at consolidation time).
    KvDelete { key: Vec<u8> },
    /// Vector index upsert. `index` is the vector index name (e.g., "chunk_emb").
    VectorUpsert { index: String, id: Vec<u8>, embedding: Vec<f32>, metadata: serde_json::Value },
    /// Graph node upsert.
    GraphNode { graph: String, id: Vec<u8>, label: String, props: serde_json::Value },
    /// Graph edge upsert.
    GraphEdge { graph: String, src: Vec<u8>, dst: Vec<u8>, rel: String, props: serde_json::Value },
}

// ----- VectorHit -----

/// A single `vector_search` result.
///
/// `rerank_applied=true` means the backend ran a cross-encoder rerank pass; backends
/// without native rerank set this to `false` and the retriever may apply rerank
/// downstream (or skip per `degraded_fallback`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorHit {
    pub id: Vec<u8>,
    pub score: f32,
    pub rerank_applied: bool,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

// ----- Filter -----

/// Filter expression passed to `vector_search`. v0 supports a small algebra; backends
/// translate to their native filter language (`FT.SEARCH ... FILTER ...` on Moon,
/// `WHERE ...` on Postgres).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Filter {
    /// Equality on a metadata field.
    Eq { field: String, value: serde_json::Value },
    /// Field value starts with this string. Used by HeliosScratchpad recipe
    /// (`source LIKE 'helios:fs/<prefix>'`).
    StartsWith { field: String, prefix: String },
    /// Logical AND of sub-filters.
    And(Vec<Filter>),
    /// Logical OR of sub-filters.
    Or(Vec<Filter>),
}

// ----- CypherQuery / GraphResult -----

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CypherQuery {
    pub graph: String,
    pub cypher: String,
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GraphResult {
    pub headers: Vec<String>,
    /// Row-major; column count must equal `headers.len()` for every row.
    pub rows: Vec<Vec<serde_json::Value>>,
}

// ----- Row<T> -----

/// A single MVCC row returned by `read_as_of`. `bt` carries the bi-temporal stamp
/// the row was visible under at the snapshot time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Row<T> {
    pub key: Vec<u8>,
    pub value: T,
    pub bt: BiTemporal,
}

// ----- QueueMsg -----

/// A single message yielded by `subscribe`. `offset` is the broker-assigned monotonic
/// offset within `(topic, partition)`.
#[derive(Clone, Debug)]
pub struct QueueMsg {
    pub topic: String,
    pub partition: u16,
    pub offset: u64,
    pub payload: Bytes,
}
