//! Supporting types referenced by the `StoragePort` trait.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::bitemporal::BiTemporal;
use crate::error::StorageError;
use crate::hlc::Hlc;
use crate::scope::Scope;

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

// ----- ScopePage -----

/// One page of scope identifiers returned by
/// [`StoragePort::list_scopes`](super::port::StoragePort::list_scopes).
///
/// `scopes` is ordered ascending by [`Scope`] string. `next_cursor` is `Some`
/// when the underlying backend has more scopes to yield — callers pass the
/// returned cursor back unchanged to fetch the next page. The cursor is an
/// **opaque** UTF-8 string (Q-U1 lock 2026-05-17 — opaque base64 of the
/// backend's native pagination handle); callers MUST NOT parse or mutate it.
///
/// `next_cursor == None` means the enumeration is exhausted; subsequent calls
/// with a `None` cursor restart from the beginning.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopePage {
    /// Scopes in this page, sorted ascending by [`Scope`] string.
    pub scopes: Vec<Scope>,
    /// Opaque continuation token; `Some` if more scopes remain.
    pub next_cursor: Option<String>,
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
#[non_exhaustive]
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

/// Sanitize free-form text into a Cypher-safe identifier for
/// [`WriteOp::GraphNode::label`] / [`WriteOp::GraphEdge::rel`].
///
/// T-01-03-01 (see `lunaris-storage-moon::atomic` / `lunaris-storage-postgres::atomic`
/// module docs): both Moon's Cypher and Postgres/AGE's Cypher-in-SQL interpolate
/// `label`/`rel` directly into the query string with no parameterization -- v0
/// trusts callers to validate against `^[A-Za-z_][A-Za-z0-9_]*$` BEFORE calling
/// `atomic_write`. Grammar-constrained extractors (Candle + GBNF) always produce
/// clean identifiers; free-form JSON extractors (Ollama, cloud-API models with no
/// schema enforcement) do not -- an entity_type of `"TV Show"` broke Moon's Cypher
/// parser in live testing (LongMemEval graph-pipeline prototype, 2026-07). Callers
/// that build `GraphNode`/`GraphEdge` from extractor or agent-supplied text (both
/// `lunaris::ingest` and `lunaris::structured_ingest`) MUST route `entity_type` /
/// `predicate` through this function first.
///
/// Every byte outside `[A-Za-z0-9_]` becomes `_`; a leading character that isn't
/// a letter or underscore gets an extra `_` prefixed (Cypher identifiers can't
/// start with a digit). An all-invalid or empty input returns `fallback` instead
/// of an empty or all-underscore string.
///
/// A shape-valid identifier can still break Moon's Cypher parser if it's a
/// whole-word match against one of Moon's reserved keywords: Moon's lexer
/// (`vendor/moon/src/graph/cypher/lexer.rs`) matches these case-insensitively
/// as exact tokens, which take priority over its identifier regex. A
/// free-form extractor emitting a predicate like `"is"` (the copula in "Tom
/// is a doctor") produces exactly this collision -- `MERGE (a)-[r:is]->(b)`
/// lexes `is` as the `IS` keyword, not an identifier, and the parser rejects
/// it (live-testing regression, LongMemEval graph-pipeline prototype,
/// 2026-07). Any sanitized result that fully matches (case-insensitively) a
/// reserved word also falls back to `fallback`.
pub fn sanitize_graph_ident(s: &str, fallback: &str) -> String {
    let mut out: String =
        s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if out.is_empty() || out.chars().all(|c| c == '_') {
        return fallback.to_string();
    }
    if !matches!(out.chars().next(), Some(c) if c.is_ascii_alphabetic() || c == '_') {
        out.insert(0, '_');
    }
    if CYPHER_RESERVED_WORDS.contains(&out.to_ascii_lowercase().as_str()) {
        return fallback.to_string();
    }
    out
}

/// Moon's Cypher keyword set (`vendor/moon/src/graph/cypher/lexer.rs`,
/// `#[token(..., ignore(case))]` variants) -- kept in sync by hand since
/// lunaris-core has no dependency on Moon's vendored Cypher module.
const CYPHER_RESERVED_WORDS: &[&str] = &[
    "match", "optional", "where", "return", "create", "delete", "detach", "set", "merge", "with",
    "unwind", "order", "by", "limit", "skip", "as", "and", "or", "not", "true", "false", "null",
    "in", "is", "call", "yield", "distinct", "asc", "desc", "on",
    // Moon v0.6.0 string-operator tokens (lexer lines 80-84). Drift from a
    // vendor bump is caught by `reserved_word_list_matches_vendored_moon_lexer`.
    "contains", "starts", "ends",
];

#[cfg(test)]
mod sanitize_graph_ident_tests {
    use super::*;

    #[test]
    fn passthrough_for_already_clean_identifiers() {
        assert_eq!(sanitize_graph_ident("Person", "Entity"), "Person");
        assert_eq!(sanitize_graph_ident("attends_school", "RELATED_TO"), "attends_school");
    }

    #[test]
    fn spaces_become_underscores() {
        // The exact live-testing regression: MiniMax emitted an entity_type of
        // "TV Show", which broke Moon's Cypher parser on the bare `Show` token.
        assert_eq!(sanitize_graph_ident("TV Show", "Entity"), "TV_Show");
    }

    #[test]
    fn punctuation_becomes_underscores() {
        assert_eq!(sanitize_graph_ident("co-worker's", "RELATED_TO"), "co_worker_s");
    }

    #[test]
    fn leading_digit_gets_prefixed() {
        assert_eq!(sanitize_graph_ident("3d_printer", "Entity"), "_3d_printer");
    }

    #[test]
    fn empty_input_falls_back() {
        assert_eq!(sanitize_graph_ident("", "Entity"), "Entity");
    }

    #[test]
    fn all_invalid_input_falls_back() {
        assert_eq!(sanitize_graph_ident("!!!", "RELATED_TO"), "RELATED_TO");
    }

    #[test]
    fn unicode_letters_become_underscores() {
        // Cypher identifiers here are ASCII-only per T-01-03-01's regex; non-ASCII
        // letters are treated the same as punctuation, not preserved lossily.
        assert_eq!(sanitize_graph_ident("café", "Entity"), "caf_");
    }

    #[test]
    fn reserved_keyword_falls_back() {
        // The exact live-testing regression: MiniMax emitted predicate="is"
        // for a copula fact ("Tom is a doctor"). "is" is shape-valid but
        // collides with Moon's `IS` keyword token.
        assert_eq!(sanitize_graph_ident("is", "RELATED_TO"), "RELATED_TO");
        assert_eq!(sanitize_graph_ident("on", "RELATED_TO"), "RELATED_TO");
        assert_eq!(sanitize_graph_ident("not", "RELATED_TO"), "RELATED_TO");
    }

    #[test]
    fn reserved_keyword_match_is_case_insensitive() {
        // Moon's lexer matches keywords via `ignore(case)`.
        assert_eq!(sanitize_graph_ident("IS", "RELATED_TO"), "RELATED_TO");
        assert_eq!(sanitize_graph_ident("Is", "RELATED_TO"), "RELATED_TO");
        assert_eq!(sanitize_graph_ident("Set", "Entity"), "Entity");
    }

    #[test]
    fn moon_v060_string_operator_keywords_fall_back() {
        // Live regression (2026-07-10 failed-cases smoke, q10): MiniMax
        // emitted relation type "Contains"; the Moon v0.6.0 lexer added
        // CONTAINS/STARTS/ENDS string-operator tokens and the hand-synced
        // list here silently drifted — Moon threw "Cypher parse error:
        // expected identifier, found Contains" and the whole ingest failed.
        assert_eq!(sanitize_graph_ident("Contains", "RELATED_TO"), "RELATED_TO");
        assert_eq!(sanitize_graph_ident("starts", "RELATED_TO"), "RELATED_TO");
        assert_eq!(sanitize_graph_ident("ENDS", "RELATED_TO"), "RELATED_TO");
    }

    #[test]
    fn reserved_word_list_matches_vendored_moon_lexer() {
        // Drift gate for the "kept in sync by hand" promise on
        // CYPHER_RESERVED_WORDS: read the vendored Moon Cypher lexer and
        // assert every `ignore(case)` keyword token is present in the list.
        // Skips when the submodule isn't checked out (published-crate
        // builds, partial clones) — the gate matters exactly where vendor
        // bumps happen: this repo's CI.
        let lexer = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/moon/src/graph/cypher/lexer.rs");
        let Ok(src) = std::fs::read_to_string(&lexer) else {
            eprintln!("skip: vendored moon lexer not present at {}", lexer.display());
            return;
        };
        let mut missing: Vec<String> = Vec::new();
        for line in src.lines() {
            let Some(rest) = line.trim().strip_prefix("#[token(b\"") else { continue };
            let Some(word) = rest.split('"').next() else { continue };
            if !rest.contains("ignore(case)") {
                continue;
            }
            let lower = word.to_ascii_lowercase();
            if !CYPHER_RESERVED_WORDS.contains(&lower.as_str()) && !missing.contains(&lower) {
                missing.push(lower);
            }
        }
        assert!(
            missing.is_empty(),
            "Moon lexer keywords missing from CYPHER_RESERVED_WORDS (vendor bump drift): {missing:?}"
        );
    }

    #[test]
    fn keyword_as_a_substring_is_not_treated_as_reserved() {
        // Only a FULL match against a reserved word collides -- a longer
        // identifier that merely starts with one lexes fine as a single
        // Ident token (maximal munch), matching Moon's real lexer behavior.
        assert_eq!(sanitize_graph_ident("is_a", "RELATED_TO"), "is_a");
        assert_eq!(sanitize_graph_ident("Island", "Entity"), "Island");
    }
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
#[non_exhaustive]
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
    /// Constrain hits to items whose `valid_time` falls within the
    /// half-open range `[after, before)`. Either bound may be `None`:
    /// - `after = None`  → open-ended left bound (matches `-inf`).
    /// - `before = None` → open-ended right bound (matches `+inf`).
    /// - both `None`     → predicate no-op (degrades to `TRUE` on SQL,
    ///   `@valid_time:[-inf +inf]` on Moon).
    ///
    /// This filter lives on a different axis from `as_of` — `as_of` pins
    /// the MVCC snapshot timestamp, whereas `ValidTimeRange` constrains
    /// which items are candidates at all. Mirrors `TemporalQuery.before /
    /// .after / .between` per Phase 9.1 Plan 02 (PRIM-03 full wiring).
    ValidTimeRange { after: Option<Hlc>, before: Option<Hlc> },
}

// ----- CypherQuery / GraphResult -----

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CypherQuery {
    pub graph: String,
    pub cypher: String,
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// Recency-decay parameters for graph traversal (ADD task `graph-decay-recency`,
/// contract v1). Maps to Moon's `GRAPH.QUERY ... --decay <λ> [--time-weight <w>]`
/// read-path clause: effective edge cost = `|weight| + λ·w·age_seconds`.
///
/// Fields are PRIVATE — validity by construction. `new` rejects non-finite or
/// negative λ; `with_time_weight` rejects non-finite or non-positive w, so a
/// time weight without a decay rate is unrepresentable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphDecay {
    lambda: f64,
    time_weight: Option<f64>,
}

impl GraphDecay {
    /// Construct with a decay rate λ. Accepts finite λ ≥ 0 only (λ = 0 is
    /// valid and decay-neutral, matching Moon's server-side validation).
    pub fn new(lambda: f64) -> Result<Self, StorageError> {
        if !lambda.is_finite() || lambda < 0.0 {
            return Err(StorageError::Backend(format!(
                "graph_decay_invalid_lambda: must be finite and >= 0, got {lambda}"
            )));
        }
        Ok(Self { lambda, time_weight: None })
    }

    /// Set the time-weight multiplier `w`. Accepts finite w > 0 only.
    pub fn with_time_weight(self, w: f64) -> Result<Self, StorageError> {
        if !w.is_finite() || w <= 0.0 {
            return Err(StorageError::Backend(format!(
                "graph_decay_invalid_time_weight: must be finite and > 0, got {w}"
            )));
        }
        Ok(Self { time_weight: Some(w), ..self })
    }

    /// The decay rate λ (finite, ≥ 0).
    pub fn lambda(&self) -> f64 {
        self.lambda
    }

    /// The time-weight multiplier, if set (finite, > 0).
    pub fn time_weight(&self) -> Option<f64> {
        self.time_weight
    }
}

/// Parameters for graph-expanded vector retrieval (ADD task
/// `ft-navigate-recall`, contract v1). Maps to Moon's
/// `FT.NAVIGATE ... HOPS <n> [HOP_PENALTY <p>] [DECAY <λ>]`.
///
/// Fields are PRIVATE — validity by construction. `hops` is bounded to
/// `1..=5` (Moon's `MAX_EXPAND_DEPTH`); `hop_penalty` must be finite ≥ 0.
///
/// Note: `FT.NAVIGATE DECAY` has no time-weight slot (the server fixes
/// `time_weight = 1.0`); a [`GraphDecay`] carrying `time_weight` sends only
/// its λ on the navigate wire — `time_weight` is a `GRAPH.QUERY`-only knob.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NavigateSpec {
    hops: u32,
    hop_penalty: Option<f64>,
    decay: Option<GraphDecay>,
}

impl NavigateSpec {
    /// Maximum hop depth Moon's graph expansion accepts (`MAX_EXPAND_DEPTH`).
    pub const MAX_HOPS: u32 = 5;

    /// Construct with a hop depth. Accepts `1..=5` only — 0 hops is plain
    /// vector search (use `vector_search`), >5 is rejected by Moon anyway.
    pub fn new(hops: u32) -> Result<Self, StorageError> {
        if hops == 0 || hops > Self::MAX_HOPS {
            return Err(StorageError::Backend(format!(
                "navigate_invalid_hops: must be 1..={}, got {hops}",
                Self::MAX_HOPS
            )));
        }
        Ok(Self { hops, hop_penalty: None, decay: None })
    }

    /// Set the per-hop score penalty (server default 0.1 when unset).
    /// Accepts finite p ≥ 0 (0 ranks graph hits purely by decay/vec score).
    pub fn with_hop_penalty(self, p: f64) -> Result<Self, StorageError> {
        if !p.is_finite() || p < 0.0 {
            return Err(StorageError::Backend(format!(
                "navigate_invalid_hop_penalty: must be finite and >= 0, got {p}"
            )));
        }
        Ok(Self { hop_penalty: Some(p), ..self })
    }

    /// Apply recency decay to the discovery edge of each graph-expanded hit.
    /// Infallible — `GraphDecay` is already validated by construction.
    pub fn with_decay(self, decay: GraphDecay) -> Self {
        Self { decay: Some(decay), ..self }
    }

    /// The hop depth (1..=5).
    pub fn hops(&self) -> u32 {
        self.hops
    }

    /// The per-hop penalty, if set (finite, ≥ 0).
    pub fn hop_penalty(&self) -> Option<f64> {
        self.hop_penalty
    }

    /// The recency decay, if set.
    pub fn decay(&self) -> Option<GraphDecay> {
        self.decay
    }
}

/// A single hit returned by `StoragePort::vector_navigate`.
///
/// `id` is the decoded doc id (FT index prefix stripped + hex-decoded, the
/// same rule as [`VectorHit`]). `hop_depth == 0` means the hit was a direct
/// KNN seed; `>= 1` means it was discovered via graph expansion. Lower
/// `final_score` = better (consistent with L2 distance ordering).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NavigateHit {
    pub id: Vec<u8>,
    pub vec_score: f32,
    pub hop_depth: u32,
    pub final_score: f32,
}

/// One entry from a backend's hot-key sketch (`StoragePort::hot_keys`).
///
/// `key` is the RAW backend key (e.g. a Moon `lunaris:{scope}:{kind}:{ulid}`
/// KV key or an FT doc HASH key) — consumers MUST treat it as untrusted
/// bytes and classify/validate before surfacing it anywhere (lunaris-server
/// never exposes raw keys; it aggregates into bounded (scope, kind) labels).
/// `sampled_count` is the backend's sampled estimate — on Moon, 1-in-64
/// sampling means multiplying by 64 approximates the true command count.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotKey {
    pub key: Vec<u8>,
    pub sampled_count: u64,
}

/// Dynamic header-keyed result table returned by `StoragePort::graph_traverse`.
///
/// `headers` carries column names in row order; `rows` are row-major with
/// `rows[i].len() == headers.len()`. The shape is **wire-additive**: backends
/// MAY append new columns without breaking older consumers, and consumers
/// SHOULD locate columns by header name (not positional index) when they need
/// optional metadata.
///
/// # Canonical columns (Plan 03-03 + P0 #4 Wave 3)
///
/// The `Graph::anchored` operator emits a Cypher RETURN whose canonical
/// header order is:
///
/// | Header                 | Type      | Meaning                                                  | Default if missing |
/// |------------------------|-----------|----------------------------------------------------------|--------------------|
/// | `id`                   | hex str   | `m.id_hex` — destination entity's 16-byte ULID, hex.     | `""` (empty hit)   |
/// | `name`                 | str/null  | `m.name` — display name.                                 | `null`             |
/// | `type`                 | str/null  | `m.type` — entity type tag.                              | `null`             |
/// | `path_length`          | int       | Number of edges traversed (`length(path)`).              | row index `i`      |
/// | `edge_weight_product`  | float     | product of `coalesce(r.weight, 1.0)` over path edges      | `1.0`              |
/// | `anchor_confidence`    | float     | Confidence of the seed entity that anchored this path.   | `1.0`              |
///
/// The first three are required by the operator today (positional read for
/// back-compat with all `canned_graph_with`-shaped test fixtures). The last
/// three are **optional**: when absent, the operator substitutes the documented
/// defaults so the scoring formula reduces to the legacy `1.0 / (1.0 + i)`.
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
            after: Some(Hlc { wall_ms: 100, counter: 0, node_id: 0 }),
            before: Some(Hlc { wall_ms: 200, counter: 5, node_id: 0 }),
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
