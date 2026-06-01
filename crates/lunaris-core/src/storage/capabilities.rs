//! `StorageCapabilities` — what the backend supports natively.
//!
//! Higher layers (retrievers, recipes, the conformance suite) read this struct to
//! decide whether to degrade gracefully (e.g., skip rerank when `rerank_native=false`)
//! or refuse a query path (e.g., reject Cypher when `graph_native=false`).

use serde::{Deserialize, Serialize};

/// Cypher dialect tier the backend's graph executor accepts.
///
/// Wave 4 amendment: parallel-agent probes against the two supported
/// backends discovered that neither accepts the full Wave-4 template:
///
/// - **Moon** rejects `MATCH p = (n)-[*1..N]-(m)` (path-variable binding
///   is restricted to `shortestPath()` calls). Its function table also
///   omits `length()`, `reduce()`, and `relationships()`. Primary-source
///   citation: `vendor/moon/src/graph/cypher/parser/pattern.rs:172-216`
///   + `executor/eval.rs:116-227`.
/// - **Apache AGE 1.5** (Postgres backend) accepts `MATCH p = ...` and
///   `length(p)` and `n.id_hex AS source_entity_id`, but rejects
///   `reduce(acc, x in xs | ...)` — parse error at the `|` token.
///   Confirmed by the Wave 4b probe.
///
/// Each backend declares the tier it genuinely supports via
/// [`StorageCapabilities::cypher_dialect`]. The
/// `crates/lunaris-retrieve/src/operators/graph.rs` operator reads this
/// field at retrieve-time and picks the matching template. Promotion to
/// a higher tier requires primary-source dialect verification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CypherDialect {
    /// Pre-Wave-4 template: `MATCH (n)-[*1..N]-(m) RETURN m.id_hex AS id,
    /// m.name AS name, m.type AS type`. No path metrics, no source
    /// entity. All backends support this. The operator's optional-header
    /// fallback (Wave 3) reduces the score formula to the legacy
    /// `1.0 / (1.0 + i)` when no path metrics arrive.
    #[default]
    Legacy,
    /// Wave 4 partial: adds `MATCH p = ...` path binding + `length(p) AS
    /// path_length` + `n.id_hex AS source_entity_id`. Omits the
    /// `reduce(...)` expression because AGE 1.5 rejects the `|` token.
    /// The operator stamps `anchor_confidence` post-Cypher from the seed
    /// map; `edge_weight_product` falls back to its `1.0` default in the
    /// header-keyed parser. AGE 1.5 compatible.
    PathMetrics,
    /// Wave 4 full: PathMetrics + `reduce(w=1.0, r in relationships(p) |
    /// w * coalesce(r.weight, 1.0)) AS edge_weight_product`. No current
    /// backend supports this — kept for forward-compat when Moon or AGE
    /// add `reduce()` over variable-length paths.
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageCapabilities {
    /// `true` only when KV reads support historical `AS_OF` natively.
    ///
    /// Moon supports temporal graph and FT surfaces, but its hash/KV `HGET`
    /// path currently returns latest state for Lunaris KV rows. Postgres
    /// stores bi-temporal columns but the Lunaris Postgres adapter still
    /// treats that as an emulated storage contract.
    pub bi_temporal_native: bool,
    /// `true` for Moon (CSR + Cypher) and Postgres (AGE extension).
    pub graph_native: bool,
    /// `true` for Moon's bundled cross-encoder; `false` for Postgres.
    pub rerank_native: bool,
    /// `true` for Moon (`MQ.*`) and Postgres (`pgmq`).
    pub queue_native: bool,
    /// Maximum vector dimension the backend's index can hold. Typically 768 (Moon
    /// EmbeddingGemma) or 1536 (Postgres+pgvector default upper bound).
    pub max_vector_dim: u32,
    /// `true` when the backend can run RRF (Reciprocal Rank Fusion) over
    /// `(vector, sparse_bm25)` natively in a single round trip — i.e., Moon's
    /// `FT.SEARCH ... HYBRID VECTOR ... SPARSE ... FUSION RRF WEIGHTS ...` exposed
    /// via `client.text().hybrid_search()`. Phase 2's `fuse_rrf` operator opts into
    /// `RrfFusion::Moon` when both branches hit a backend with `native_rrf=true` AND
    /// both branches are `Vector` / `Keyword(BM25)` operators on the same Moon index.
    /// Backends with `native_rrf=false` (e.g., Postgres) fall back to client-side
    /// fusion (`RrfFusion::Client`).
    pub native_rrf: bool,
    /// Recommended upper bound on active scopes for this backend.
    ///
    /// Moon creates one FT index + one graph key + N MQ topics **per scope**.
    /// Moon's soft limit is ~512 FT indices per node before recall p99 degrades
    /// (per Moon docs §6.4 "index count"). Above `max_scopes_recommended` the
    /// operator should consider workspace-level pooling (future RFC). A value of
    /// `0` means no limit is documented (e.g., Postgres with RLS has no index
    /// multiplier per scope).
    ///
    /// RFC 0001 §3.6: set to `512` for Moon, `0` for Postgres.
    pub max_scopes_recommended: usize,
    /// Cypher dialect tier the backend's graph executor accepts.
    ///
    /// Defaults to [`CypherDialect::Legacy`] — the universally-supported
    /// pre-Wave-4 template. Backends opt into higher tiers only with
    /// primary-source dialect verification:
    /// - Moon: stays at `Legacy` (Wave 4 probe rejected `MATCH p = ...`).
    /// - Postgres (AGE 1.5): `PathMetrics` (accepts `MATCH p = ...` +
    ///   `length(p)` + `source_entity_id`; rejects `reduce(...)`).
    /// - Embedded: `Legacy` (no graph support; returns `NotSupported`).
    ///
    /// See [`CypherDialect`] for the full tier matrix.
    pub cypher_dialect: CypherDialect,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cypher_dialect_default_is_legacy() {
        // Default tier MUST be Legacy — universally-supported template.
        // Backends opt into higher tiers explicitly; new backends (or
        // test fixtures that build via `..Default::default()`) MUST NOT
        // silently promote.
        assert_eq!(CypherDialect::default(), CypherDialect::Legacy);
    }
}
