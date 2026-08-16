//! `StorageCapabilities` — what the backend supports natively.
//!
//! Higher layers (retrievers, recipes, the conformance suite) read this struct to
//! decide whether to degrade gracefully (e.g., skip rerank when `rerank_native=false`)
//! or refuse a query path (e.g., reject Cypher when `graph_native=false`).

use serde::{Deserialize, Serialize};

/// Cypher dialect tier the backend's graph executor accepts.
///
/// Wave 4 amendment: a parallel-agent probe found that Moon does not
/// accept the full Wave-4 template. It rejects
/// `MATCH p = (n)-[*1..N]-(m)` (path-variable binding is restricted to
/// `shortestPath()` calls) and its function table omits `length()`,
/// `reduce()`, and `relationships()`. Primary-source citation:
/// `vendor/moon/src/graph/cypher/parser/pattern.rs:172-216`
/// + `executor/eval.rs:116-227`.
///
/// Each backend declares the tier it genuinely supports via
/// [`StorageCapabilities::cypher_dialect`]. The
/// `crates/lunaris-retrieve/src/operators/graph.rs` operator reads this
/// field at retrieve-time and picks the matching template. Promotion to
/// a higher tier requires primary-source dialect verification.
///
/// 0.7.0 removed the intermediate `PathMetrics` tier — it existed only to
/// describe Apache AGE 1.5 (path binding + `length(p)`, but no `reduce()`),
/// and the Postgres backend that spoke it was deleted with the Moon-only
/// break. No producer survived it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CypherDialect {
    /// Pre-Wave-4 template: `MATCH (n)-[*1..N]-(m) RETURN m.id_hex AS id,
    /// m.name AS name, m.type AS type`. No path metrics, no source
    /// entity. This is Moon's ceiling today. The operator's
    /// optional-header fallback (Wave 3) reduces the score formula to the
    /// legacy `1.0 / (1.0 + i)` when no path metrics arrive.
    #[default]
    Legacy,
    /// Wave 4 full: `MATCH p = ...` path binding + `length(p) AS
    /// path_length` + `n.id_hex AS source_entity_id` +
    /// `reduce(w=1.0, r in relationships(p) | w * coalesce(r.weight, 1.0))
    /// AS edge_weight_product`. No current backend supports this — kept
    /// for forward-compat against a Moon that grows path binding and
    /// `reduce()` over variable-length paths.
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
    /// pre-Wave-4 template. Backends opt into `Full` only with
    /// primary-source dialect verification; Moon stays at `Legacy` (the
    /// Wave 4 probe rejected `MATCH p = ...`).
    ///
    /// See [`CypherDialect`] for the full tier matrix.
    pub cypher_dialect: CypherDialect,
    /// `true` when the backend's graph executor supports native recency-decay
    /// traversal — i.e., Moon's `GRAPH.QUERY ... --decay <λ> [--time-weight <w>]`
    /// read-path clause (effective edge cost `|weight| + λ·w·age_seconds`).
    ///
    /// Callers gate `StoragePort::graph_traverse_decayed(decay: Some(_))` on
    /// this flag; the default trait impl returns `NotSupported`. `true` for
    /// Moon (v0.3.0+); `false` for Postgres (AGE has no decay clause) and
    /// embedded. `#[serde(default)]` keeps pre-v0.7 serialized capability
    /// payloads parseable (missing field → `false`).
    #[serde(default)]
    pub graph_decay_native: bool,
    /// `true` when the backend supports graph-expanded vector retrieval —
    /// i.e., Moon's `FT.NAVIGATE` (KNN seeds → bounded BFS → hop-aware
    /// re-rank). Callers gate `StoragePort::vector_navigate` on this flag;
    /// the DSL `Navigate` operator degrades to plain `vector_search` when
    /// `false`. `true` for Moon (v0.3.0+); `false` for Postgres/embedded.
    /// `#[serde(default)]` keeps older serialized payloads parseable.
    #[serde(default)]
    pub graph_navigate_native: bool,
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
