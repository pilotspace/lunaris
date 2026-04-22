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

/// Plan 08-02 additive: `Display` lets the PyO3 / napi-rs bindings emit
/// `Lsn::to_string()` as the FFI-crossing shape for `ingest() -> Lsn` and
/// `snapshot() -> Lsn`. The format is `"{wall_ms}:{counter}"` — stable,
/// total-order-preserving under lexicographic compare IFF wall_ms/counter
/// are zero-padded to fixed widths; we deliberately do NOT zero-pad because
/// callers that need ordering compare the numeric fields directly (Lsn
/// derives `PartialOrd + Ord`). The string form is intended for opaque
/// display / serialisation only.
impl std::fmt::Display for Lsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.wall_ms, self.counter)
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

// ----- RrfFusion -----

/// RRF (Reciprocal Rank Fusion) mode for the retrieval DSL.
///
/// Phase 2's `fuse_rrf` operator chooses between client-side fusion and
/// Moon-native `text().hybrid_search()` based on this hint and the backend's
/// [`StorageCapabilities::native_rrf`](super::capabilities::StorageCapabilities)
/// flag. Phase 1.5 (STORE-09) ships the type + the capability bit; Phase 2
/// wires the operator that consumes them.
///
/// # Example
///
/// ```
/// use lunaris_core::RrfFusion;
///
/// // Always available; works on any StoragePort backend (incl. Postgres).
/// let client_side = RrfFusion::Client { k: 60 };
///
/// // Only valid when capabilities().native_rrf == true (Moon backend) AND
/// // both branches are Vector + Keyword(BM25) on the same Moon index.
/// let moon_native = RrfFusion::Moon { k: 60, weights: [0.5, 0.5] };
/// match moon_native {
///     RrfFusion::Moon { k, weights } => {
///         assert_eq!(k, 60);
///         assert_eq!(weights, [0.5, 0.5]);
///     }
///     _ => unreachable!(),
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RrfFusion {
    /// Client-side fusion: collect both branches' results, apply `1 / (k + rank)`
    /// per branch, sum scores. Works on any [`StoragePort`](super::port::StoragePort)
    /// backend regardless of `capabilities().native_rrf`.
    Client { k: usize },
    /// Moon-native fusion: invoke
    /// `FT.SEARCH ... HYBRID VECTOR ... SPARSE ... FUSION RRF WEIGHTS <bm25_w> <vec_w>`
    /// in a single round trip via `client.text().hybrid_search()`. Only valid when
    /// `capabilities().native_rrf == true` AND both branches are `Vector` and
    /// `Keyword(BM25)` operators on the same Moon index. Phase 2's `fuse_rrf`
    /// query planner picks this variant for Moon backends; falls back to
    /// `Client` on non-native backends (e.g., Postgres).
    Moon {
        /// RRF constant in the `1 / (k + rank)` formula. Conventional value: 60.
        k: usize,
        /// Branch weights `[bm25_weight, vector_weight]`. Both must be non-negative
        /// and finite (moon-client's `text().hybrid_search` rejects negative or
        /// `NaN`/`Inf` weights). Conventional balanced value: `[0.5, 0.5]`.
        weights: [f64; 2],
    },
}

#[cfg(test)]
mod filter_valid_time_range_tests {
    use super::*;

    #[test]
    fn filter_valid_time_range_roundtrip() {
        let f = Filter::ValidTimeRange {
            after: Some(Hlc { wall_ms: 100, counter: 0 }),
            before: Some(Hlc { wall_ms: 200, counter: 5 }),
        };
        let s = serde_json::to_string(&f).expect("serialize ValidTimeRange");
        let parsed: Filter = serde_json::from_str(&s).expect("deserialize ValidTimeRange");
        assert!(matches!(parsed, Filter::ValidTimeRange { after: Some(_), before: Some(_) }));
    }

    #[test]
    fn filter_valid_time_range_both_none() {
        let f = Filter::ValidTimeRange { after: None, before: None };
        let s = serde_json::to_string(&f).expect("serialize both-None");
        let parsed: Filter = serde_json::from_str(&s).expect("deserialize both-None");
        assert!(matches!(parsed, Filter::ValidTimeRange { after: None, before: None }));
    }

    #[test]
    fn filter_existing_variants_unchanged() {
        let eq = Filter::Eq { field: "source".into(), value: serde_json::json!("helios:fs/") };
        let sw = Filter::StartsWith { field: "source".into(), prefix: "helios:fs/".into() };
        let and_f = Filter::And(vec![eq.clone(), sw.clone()]);
        let or_f = Filter::Or(vec![eq.clone(), sw.clone()]);
        for f in [eq, sw, and_f, or_f] {
            let s = serde_json::to_string(&f).expect("serialize existing variant");
            let _: Filter = serde_json::from_str(&s).expect("deserialize existing variant");
        }
    }
}

#[cfg(test)]
mod rrf_fusion_tests {
    use super::*;

    #[test]
    fn rrf_fusion_moon_constructible() {
        let m = RrfFusion::Moon { k: 60, weights: [0.5, 0.5] };
        match m {
            RrfFusion::Moon { k, weights } => {
                assert_eq!(k, 60);
                assert_eq!(weights, [0.5, 0.5]);
            }
            _ => panic!("expected Moon variant"),
        }
    }

    #[test]
    fn rrf_fusion_client_constructible() {
        let c = RrfFusion::Client { k: 60 };
        match c {
            RrfFusion::Client { k } => assert_eq!(k, 60),
            _ => panic!("expected Client variant"),
        }
    }

    #[test]
    fn rrf_fusion_moon_and_client_are_distinct() {
        // Equal `k` must NOT make Client and Moon compare equal — the planner
        // distinguishes the two variants by shape, not by k alone.
        let c = RrfFusion::Client { k: 60 };
        let m = RrfFusion::Moon { k: 60, weights: [0.5, 0.5] };
        assert_ne!(c, m);
    }

    #[test]
    fn rrf_fusion_is_copy_clone_debug() {
        // The trait bounds are part of the public contract — Phase 2's planner
        // copies the enum out of a `&Operator` reference frequently and prints
        // it in tracing spans.
        fn assert_traits<T: Copy + Clone + std::fmt::Debug + PartialEq>() {}
        assert_traits::<RrfFusion>();
    }
}
