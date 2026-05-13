//! `Graph::anchored(entity_ids, hops)` — graph-anchored retrieval (RETRIEVE-03).
//!
//! Per blueprint §8 + ROADMAP Phase 3 success criterion #3, the operator starts
//! from a list of pre-resolved [`EntityId`]s (extracted from the query text by
//! the planner per D-13), traverses the graph via Cypher BFS for `hops` steps,
//! and returns reachable entity ids as `RawHit { source_op: SourceOp::Graph }`.
//!
//! ## Portable Cypher template (D-16, Wave 4)
//!
//! Runs against Moon `client.graph().query_with_params(graph, cypher,
//! params_json)` AND Postgres AGE `SELECT * FROM cypher('lunaris_graph', $$ ...
//! $$, $1)` without per-backend translation:
//!
//! ```text
//! UNWIND $ids AS sid
//! MATCH p = (n {id_hex: sid})-[*1..<hops>]-(m)
//! RETURN
//!   m.id_hex   AS id,
//!   m.name     AS name,
//!   m.type     AS type,
//!   length(p)  AS path_length,
//!   reduce(w=1.0, r in relationships(p) | w * coalesce(r.weight, 1.0))
//!              AS edge_weight_product,
//!   n.id_hex   AS source_entity_id
//! LIMIT $k
//! ```
//!
//! W-7 fix: the property name MUST be `id_hex` because Plan 03-03's ingest
//! fan-out writes `WriteOp::GraphNode { props: { "id_hex": format!("{}",
//! entity_id), ... } }`. Earlier drafts used `id`, which silently returned
//! zero rows because the node property is `id_hex`, not `id`.
//!
//! Wave 4 changes:
//! - `MATCH p = ...` binds the path so `length(p)` + `relationships(p)` are
//!   available in the RETURN clause.
//! - `DISTINCT` is removed: per-path metrics (`path_length`,
//!   `edge_weight_product`) would collapse under DISTINCT, and a node
//!   reachable by two distinct paths is a legitimate two-row result.
//! - Three new RETURN columns feed the Wave-3 scoring formula
//!   `(edge_weight_product / (1.0 + path_length)) * anchor_confidence`.
//! - `anchor_confidence` is NOT a Cypher column — the operator synthesizes
//!   it post-Cypher by joining `source_entity_id` against the planner's
//!   per-seed confidence map (built from `Graph::anchored`'s
//!   `Vec<(EntityId, f32)>` argument).
//!
//! Back-compat: if a backend hasn't been upgraded (the agents owning
//! `lunaris-storage-postgres/src/graph.rs` and
//! `lunaris-storage-moon/src/graph.rs` ship in parallel PRs), GraphResult
//! still arrives with only `id`/`name`/`type`. The operator detects missing
//! `path_length` / `edge_weight_product` / `source_entity_id` headers and
//! falls back to the pre-Wave-4 legacy score `1.0 / (1.0 + i)`. The
//! `graph_score_back_compat_when_no_new_headers` test pins this property.
//!
//! `<hops>` is spliced into the cypher string at construction time — the
//! openCypher spec requires variable-length pattern bounds (`[*lo..hi]`) to
//! be literals; both Moon GRAPH.QUERY and Apache AGE refuse `[*1..$hops]`.
//! `$ids` and `$k` are typed parameters so EntityIds NEVER reach the cypher
//! string text — defends T-03-02-01 (Cypher injection) by mirroring the
//! Plan 02-02 plainto_tsquery parameter-binding pattern.
//!
//! ## Defense layers (T-03-02-02 DoS mitigation)
//!
//! - `MAX_GRAPH_HOPS = 5` — hard cap on hops per D-14 (caller-supplied `hops
//!   greater than 5` clamps to 5; `hops == 0` clamps up to 1 so the BFS
//!   always runs at least one edge step).
//! - [`super::clamp_k`] caps the result count at `MAX_K = 1000` (Plan 02-02
//!   convention).
//! - Phase 4 OPS-05 will wire `tower::timeout` around the Retriever for
//!   callers that need a wall-clock cap — bounded fan-out per query.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use lunaris_core::{CypherQuery, LunarisError};
use lunaris_extract::EntityId;

use super::{QueryContext, Retriever, clamp_k};
use crate::types::{RawHit, SourceOp};

/// Default hops per ROADMAP Phase 3 success criterion #3.
pub const DEFAULT_GRAPH_HOPS: usize = 2;
/// Hard cap on hops to prevent runaway BFS (D-14). Caller-supplied
/// `hops greater than 5` clamps to 5; `hops == 0` clamps up to 1 so the BFS
/// always runs at least one edge step (zero-hop traversal would only return
/// the anchor entities themselves, which the caller already has).
pub const MAX_GRAPH_HOPS: usize = 5;
/// Default candidate count for graph traversal (matches Vector/Keyword default of 30).
pub const DEFAULT_GRAPH_K: usize = 30;
/// Graph name used by both Moon and Postgres backends. Matches the AGE
/// `SELECT create_graph('lunaris_graph')` setup in the Phase 1 Postgres
/// migration AND the Moon `GRAPH.QUERY <graph> "..."` graph parameter — both
/// backends share the same graph identifier so the Cypher template stays
/// portable.
pub const LUNARIS_GRAPH_NAME: &str = "lunaris_graph";

/// Graph-anchored retrieval operator.
///
/// Construction is fluent — wire at `recall()`-build time, the network call
/// only happens at `.retrieve()` time. The `entity_ids` come from the
/// RETRIEVE-13 planner stub (extracted from query text) per D-13. Empty
/// `entity_ids` short-circuits to an empty result set without touching
/// storage (the planner returns no entities when the query has no entity
/// mentions; treat that as "graph branch contributes nothing" rather than an
/// error).
#[derive(Clone, Debug)]
#[must_use = "Graph is a query node — pass it to RetrievalBuilder::with_root() or chain via .and/.or/.fuse_rrf, otherwise it never executes"]
pub struct Graph {
    /// Anchor seeds with per-seed confidence in `[0.0, 1.0]`. The operator
    /// snapshots a `HashMap<EntityId, f32>` from this vector at construction
    /// for O(1) confidence lookup after Cypher returns.
    pub seeds: Vec<(EntityId, f32)>,
    pub hops: usize,
    pub k: usize,
    pub graph: String,
    /// Per-seed confidence keyed by `EntityId` for O(1) post-Cypher lookup
    /// against the `source_entity_id` column. Built from `seeds` at
    /// construction time. On duplicate seed keys the MAX confidence wins
    /// (safest default for caller mistakes — a low-confidence duplicate
    /// must never demote a high-confidence anchor). Values are clamped to
    /// `[0.0, 1.0]` to prevent a caller-passed out-of-range value from
    /// poisoning the score downstream.
    pub(crate) confidence_by_seed: HashMap<EntityId, f32>,
}

impl Graph {
    /// `Graph::anchored(seeds, hops)` — primary constructor per D-13.
    ///
    /// `seeds` carries `(EntityId, confidence)` pairs where `confidence` is
    /// the planner's belief that the entity is actually relevant to the
    /// query. Values are clamped to `[0.0, 1.0]`. Duplicates collapse to
    /// MAX (safe default — never demote a high-confidence anchor by passing
    /// a low-confidence duplicate).
    ///
    /// `hops` is clamped to `[1, MAX_GRAPH_HOPS]` (D-14):
    /// - `hops` greater than 5 — clamped to 5 (DoS defense; bounded BFS fan-out)
    /// - `hops == 0` — clamped up to 1 (zero-hop traversal would only return
    ///   the anchor entities, which the caller already has)
    ///
    /// `k` defaults to [`DEFAULT_GRAPH_K`] (matches Vector/Keyword default of
    /// 30) and is clamped via [`super::clamp_k`].
    pub fn anchored(seeds: Vec<(EntityId, f32)>, hops: usize) -> Self {
        let mut confidence_by_seed: HashMap<EntityId, f32> = HashMap::with_capacity(seeds.len());
        for (id, conf) in &seeds {
            let clamped = conf.clamp(0.0, 1.0);
            confidence_by_seed
                .entry(*id)
                .and_modify(|prev| {
                    if clamped > *prev {
                        *prev = clamped;
                    }
                })
                .or_insert(clamped);
        }
        Self {
            seeds,
            hops: hops.clamp(1, MAX_GRAPH_HOPS),
            k: clamp_k(DEFAULT_GRAPH_K),
            graph: LUNARIS_GRAPH_NAME.into(),
            confidence_by_seed,
        }
    }

    /// Convenience: list of seed [`EntityId`]s (without confidence). Mirrors
    /// the pre-Wave-4 `entity_ids` field for callers that only need the keys.
    pub fn entity_ids(&self) -> Vec<EntityId> {
        self.seeds.iter().map(|(id, _)| *id).collect()
    }

    /// Override the candidate count. Clamped at [`super::MAX_K`].
    pub fn with_k(mut self, k: usize) -> Self {
        self.k = clamp_k(k);
        self
    }

    /// Override the graph name (defaults to [`LUNARIS_GRAPH_NAME`]). Useful
    /// for tenant-isolated deployments that scope each tenant to its own
    /// AGE / Moon graph.
    pub fn with_graph(mut self, graph: impl Into<String>) -> Self {
        self.graph = graph.into();
        self
    }

    // Mirror vector.rs lines 37-67 exactly — same chain method shape so the
    // canonical compose example
    // `Vector::new(...).and(Graph::anchored(...)).fuse_rrf(60).top(5)` works
    // regardless of operator order.

    /// Concurrent fan-out — runs THIS graph branch alongside `other` and
    /// concatenates results (per-source ranking preserved via
    /// [`SourceOp`]). Downstream `fuse_rrf` groups by `source_op` to fold
    /// per-branch rankings.
    pub fn and<R: Retriever + 'static>(self, other: R) -> super::combinators::AndRetriever {
        super::combinators::AndRetriever::new(Box::new(self), Box::new(other))
    }
    /// Concurrent fan-out — runs THIS graph branch alongside `other` and
    /// unions results by id (max score wins on duplicate id).
    pub fn or<R: Retriever + 'static>(self, other: R) -> super::combinators::OrRetriever {
        super::combinators::OrRetriever::new(Box::new(self), Box::new(other))
    }
    /// Sequential narrow — runs THIS graph branch first, then passes its hit
    /// ids as a `Filter::Or(Filter::Eq{id, …})` to `other`.
    pub fn then<R: Retriever + 'static>(self, other: R) -> super::combinators::ThenRetriever {
        super::combinators::ThenRetriever::new(Box::new(self), Box::new(other))
    }
    /// Cap the final result set after the graph traversal resolves.
    pub fn top(self, n: usize) -> super::modifiers::TopRetriever {
        super::modifiers::TopRetriever::new(Box::new(self), n)
    }

    /// Wrap with a cross-encoder rerank pass (Plan 02-03).
    pub fn rerank(
        self,
        reranker: Arc<dyn lunaris_rerank::Reranker>,
    ) -> super::rerank::RerankRetriever {
        super::rerank::RerankRetriever::new(Box::new(self), reranker)
    }

    /// Wrap with a fallback retriever — if THIS graph path errors (e.g.,
    /// Postgres AGE returns `NotSupported`, Moon GRAPH.QUERY transient
    /// disconnect), switch to `fallback` and tag returned hits with
    /// `degraded: true` (Plan 02-03).
    pub fn degraded_fallback<R: Retriever + 'static>(
        self,
        fallback: R,
    ) -> super::degraded::DegradedFallbackRetriever {
        super::degraded::DegradedFallbackRetriever::new(Box::new(self), Box::new(fallback))
    }

    /// Build the parameterized [`CypherQuery`] for this operator.
    ///
    /// `hops` is spliced into the cypher string literal because openCypher
    /// requires variable-length pattern bounds to be literals (both Moon and
    /// AGE refuse `[*1..$hops]`). The literal is bounded by `MAX_GRAPH_HOPS`
    /// at construction time so a caller cannot inject `[*1..1000000]`.
    ///
    /// `$ids` carries the EntityIds as hex strings — they are NEVER spliced
    /// into the cypher text (T-03-02-01 mitigation). `$k` carries the row
    /// limit. This pattern mirrors the Plan 02-02 `plainto_tsquery($1)`
    /// parameter-binding mitigation for tsquery DSL injection.
    fn build_cypher(&self) -> CypherQuery {
        // W-7 fix: MATCH and RETURN MUST use `id_hex` to align with Plan 03-03's
        // GraphNode props which writes `id_hex`. Using `id` would silently
        // return zero rows because the node property is `id_hex` not `id`.
        // Wave 4: bind the path as `p` so length(p) + relationships(p) are
        // available in RETURN. DISTINCT is removed — per-path metrics would
        // collapse under it. The `source_entity_id` column carries the seed
        // that anchored each path so the operator can join against
        // `confidence_by_seed` post-Cypher.
        let cypher = format!(
            "UNWIND $ids AS sid \
             MATCH p = (n {{id_hex: sid}})-[*1..{hops}]-(m) \
             RETURN \
               m.id_hex AS id, \
               m.name AS name, \
               m.type AS type, \
               length(p) AS path_length, \
               reduce(w=1.0, r in relationships(p) | w * coalesce(r.weight, 1.0)) AS edge_weight_product, \
               n.id_hex AS source_entity_id \
             LIMIT $k",
            hops = self.hops,
        );
        let id_strs: Vec<serde_json::Value> = self
            .seeds
            .iter()
            // EntityId Display = hex (16 bytes → 32 hex chars). Stable across
            // calls — see lunaris_extract::types::EntityId::Display impl.
            .map(|(id, _)| serde_json::Value::String(format!("{}", id)))
            .collect();
        let mut params = serde_json::Map::new();
        params.insert("ids".into(), serde_json::Value::Array(id_strs));
        params.insert("k".into(), serde_json::Value::Number(self.k.into()));
        CypherQuery { graph: self.graph.clone(), cypher, params }
    }
}

#[async_trait]
impl Retriever for Graph {
    async fn retrieve(&self, ctx: &QueryContext) -> Result<Vec<RawHit>, LunarisError> {
        if self.seeds.is_empty() {
            // No anchor entities → empty result. The planner returns no
            // entity_ids when the query has no entity mentions; treat that
            // as "graph branch contributes nothing" rather than an error.
            // Importantly, we do NOT call ctx.storage.graph_traverse() — the
            // empty case must NOT trigger a backend round trip.
            return Ok(Vec::new());
        }
        let q = self.build_cypher();
        // Wave 2.5C: use ctx.scope — plumbed from RetrievalBuilder::with_scope
        // (set by ScopedLunaris::recall/dsl) so graph_traverse is scope-isolated
        // at the storage layer. Bare Lunaris::recall() uses Scope::dev().
        let result = ctx.storage.graph_traverse(&ctx.scope, &q, ctx.query.as_of).await?;

        // P0 #4 Wave 3 + Wave 4 — Real graph scoring.
        //
        // Score formula:
        //     score = (edge_weight_product / (1.0 + path_length)) * anchor_confidence
        //
        // - `path_length` and `edge_weight_product` are read by HEADER NAME
        //   from GraphResult (additive: backends that haven't been upgraded
        //   to Wave 4 omit the headers; the operator falls back to
        //   `i / 1.0` so the score reduces to the pre-Wave-3 legacy
        //   `1.0 / (1.0 + i)`).
        // - `source_entity_id` (Wave 4) is also header-keyed. When present,
        //   the operator decodes it back to an [`EntityId`] and looks up
        //   `confidence_by_seed` for the seed-supplied confidence (Wave 4
        //   piece A — `EntityId` confidence plumbing). When absent OR when
        //   the decoded id is not in the seed map, anchor_confidence
        //   defaults to `1.0` — preserving the back-compat property.
        //
        // The first three columns (`id`/`name`/`type`) stay positional —
        // 30+ existing test fixtures and the `canned_graph_with` helper build
        // rows positionally; switching them to header lookup would break the
        // whole fixture surface.
        //
        // Defensive parsing (T-03-02-04 mitigation against malformed
        // GraphResult): row.first().and_then(...).unwrap_or("") + hex::decode
        // .unwrap_or_default() — a malformed row produces an empty-id RawHit
        // rather than panicking. Hit count never exceeds result.rows.len().
        let path_len_idx = result.headers.iter().position(|h| h == "path_length");
        let edge_w_idx = result.headers.iter().position(|h| h == "edge_weight_product");
        let source_id_idx = result.headers.iter().position(|h| h == "source_entity_id");

        let hits: Vec<RawHit> = result
            .rows
            .into_iter()
            .enumerate()
            .map(|(i, row)| {
                let id_hex = row.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
                let id_bytes = hex::decode(&id_hex).unwrap_or_default();
                let name = row.get(1).cloned().unwrap_or(serde_json::Value::Null);
                let typ = row.get(2).cloned().unwrap_or(serde_json::Value::Null);

                // Header-keyed optional columns. f64 inputs are clamped at the
                // f32 conversion boundary; non-numeric / out-of-range cells
                // fall back to defaults rather than poisoning the score.
                let path_length = path_len_idx
                    .and_then(|idx| row.get(idx))
                    .and_then(|v| v.as_f64())
                    .map(|x| x as f32)
                    .unwrap_or(i as f32);
                let edge_weight_product = edge_w_idx
                    .and_then(|idx| row.get(idx))
                    .and_then(|v| v.as_f64())
                    .map(|x| x as f32)
                    .unwrap_or(1.0);

                // Wave 4: synthesize anchor_confidence by joining
                // `source_entity_id` against the per-seed confidence map.
                // Missing column → default 1.0 (back-compat). Decode failure
                // OR seed-not-in-map → default 1.0 (defensive; a row whose
                // anchor we can't identify is treated as full-confidence
                // rather than zero-confidence, which would silently zero out
                // a legitimate hit).
                let anchor_confidence = source_id_idx
                    .and_then(|idx| row.get(idx))
                    .and_then(|v| v.as_str())
                    .and_then(EntityId::from_hex)
                    .and_then(|seed| self.confidence_by_seed.get(&seed).copied())
                    .unwrap_or(1.0);

                let score = (edge_weight_product / (1.0 + path_length)) * anchor_confidence;

                RawHit {
                    id: id_bytes,
                    score,
                    rerank_applied: false,
                    degraded: false,
                    metadata: serde_json::json!({"name": name, "type": typ}),
                    source_op: SourceOp::Graph,
                }
            })
            .collect();

        Ok(hits)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand for the empty-seeds case in tests. Avoids `Vec::<(EntityId, f32)>::new()`
    /// boilerplate when the test only cares about non-seed constructor behavior
    /// (hops clamp, k default, graph default).
    fn no_seeds() -> Vec<(EntityId, f32)> {
        Vec::new()
    }

    #[test]
    fn anchored_clamps_hops_at_5() {
        // D-14: hops > MAX_GRAPH_HOPS (5) clamps down to 5.
        let g = Graph::anchored(no_seeds(), 100);
        assert_eq!(g.hops, MAX_GRAPH_HOPS);
        let g = Graph::anchored(no_seeds(), 6);
        assert_eq!(g.hops, MAX_GRAPH_HOPS);
        let g = Graph::anchored(no_seeds(), 5);
        assert_eq!(g.hops, MAX_GRAPH_HOPS);
    }

    #[test]
    fn anchored_clamps_hops_at_least_1() {
        // hops == 0 clamps UP to 1 — zero-hop traversal would only return the
        // anchor entities themselves (which the caller already has).
        let g = Graph::anchored(no_seeds(), 0);
        assert_eq!(g.hops, 1);
    }

    #[test]
    fn anchored_default_k_is_30() {
        // Matches Vector / Keyword default of 30.
        let g = Graph::anchored(no_seeds(), 2);
        assert_eq!(g.k, DEFAULT_GRAPH_K);
    }

    #[test]
    fn with_k_clamps_at_max() {
        // T-02-02-03 — clamp_k caps at super::MAX_K to defend against
        // k = 1_000_000 DoS.
        let g = Graph::anchored(no_seeds(), 2).with_k(usize::MAX);
        assert_eq!(g.k, super::super::MAX_K);
    }

    #[test]
    fn with_graph_overrides_default() {
        let g = Graph::anchored(no_seeds(), 2).with_graph("tenant_42_graph");
        assert_eq!(g.graph, "tenant_42_graph");
    }

    #[test]
    fn anchored_default_graph_is_lunaris_graph() {
        let g = Graph::anchored(no_seeds(), 2);
        assert_eq!(g.graph, LUNARIS_GRAPH_NAME);
        assert_eq!(g.graph, "lunaris_graph");
    }

    #[test]
    fn anchored_clamps_confidence_into_unit_interval() {
        // Out-of-range confidence values must NOT poison the score. Clamp to
        // [0.0, 1.0] at the constructor boundary.
        let id_high = EntityId::from_name_and_type("High", "Person");
        let id_neg = EntityId::from_name_and_type("Neg", "Person");
        let g = Graph::anchored(vec![(id_high, 100.0), (id_neg, -5.0)], 2);
        assert_eq!(g.confidence_by_seed.get(&id_high), Some(&1.0));
        assert_eq!(g.confidence_by_seed.get(&id_neg), Some(&0.0));
    }

    #[test]
    fn anchored_duplicate_seeds_collapse_to_max_confidence() {
        // Duplicate seeds in the input vector must collapse to the MAX
        // confidence — a caller-supplied low-confidence duplicate MUST NOT
        // demote a high-confidence anchor.
        let id = EntityId::from_name_and_type("Alice", "Person");
        let g = Graph::anchored(vec![(id, 0.2), (id, 0.9), (id, 0.5)], 2);
        assert_eq!(g.confidence_by_seed.get(&id), Some(&0.9));
    }

    #[test]
    fn build_cypher_splices_hops_literal_and_parameterizes_ids_and_k() {
        let id1 = EntityId([1u8; 16]);
        let id2 = EntityId([2u8; 16]);
        let g = Graph::anchored(vec![(id1, 1.0), (id2, 1.0)], 3);
        let q = g.build_cypher();
        // Hops literal MUST be spliced into the cypher (Cypher requires
        // variable-length pattern bounds to be literals per openCypher).
        assert!(q.cypher.contains("[*1..3]"), "hops literal must be spliced: {}", q.cypher);
        // EntityIds must NOT appear in the cypher text (T-03-02-01 mitigation).
        let id1_hex = format!("{}", id1);
        let id2_hex = format!("{}", id2);
        assert!(
            !q.cypher.contains(&id1_hex),
            "entity id 1 must NOT be in cypher text: {}",
            q.cypher
        );
        assert!(
            !q.cypher.contains(&id2_hex),
            "entity id 2 must NOT be in cypher text: {}",
            q.cypher
        );
        // ... they must be in params instead.
        let ids = q.params.get("ids").expect("ids param present").as_array().unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], serde_json::Value::String(id1_hex));
        assert_eq!(ids[1], serde_json::Value::String(id2_hex));
        // k MUST be a parameter.
        assert!(q.params.contains_key("k"));
        assert_eq!(q.params.get("k").unwrap().as_u64(), Some(DEFAULT_GRAPH_K as u64));
        // hops MUST NOT be a parameter (Cypher requires a literal for the
        // variable-length pattern bound).
        assert!(!q.params.contains_key("hops"), "hops MUST NOT be a parameter");
    }

    #[test]
    fn build_cypher_uses_id_hex_property_name_not_id() {
        // W-7 fix: MATCH and RETURN MUST use `id_hex` to align with
        // Plan 03-03's GraphNode props writes. Using `id` would silently
        // return zero rows.
        let id = EntityId::from_name_and_type("Alice", "Person");
        let g = Graph::anchored(vec![(id, 1.0)], 2);
        let q = g.build_cypher();
        assert!(
            q.cypher.contains("MATCH p = (n {id_hex: sid})"),
            "MATCH must bind path p and use id_hex property: {}",
            q.cypher
        );
        assert!(
            q.cypher.contains("m.id_hex AS id"),
            "RETURN must select m.id_hex AS id: {}",
            q.cypher
        );
    }

    #[test]
    fn build_cypher_emits_wave4_columns() {
        // Wave 4: Cypher RETURN now emits path_length, edge_weight_product,
        // source_entity_id alongside id/name/type. DISTINCT is removed (would
        // collapse per-path metrics).
        let id = EntityId::from_name_and_type("Alice", "Person");
        let g = Graph::anchored(vec![(id, 1.0)], 2);
        let q = g.build_cypher();
        assert!(
            !q.cypher.contains("DISTINCT"),
            "DISTINCT must be removed (it would collapse per-path metrics): {}",
            q.cypher
        );
        assert!(
            q.cypher.contains("length(p) AS path_length"),
            "RETURN must include length(p) AS path_length: {}",
            q.cypher
        );
        assert!(
            q.cypher.contains("edge_weight_product"),
            "RETURN must include edge_weight_product: {}",
            q.cypher
        );
        assert!(
            q.cypher.contains("n.id_hex AS source_entity_id"),
            "RETURN must include n.id_hex AS source_entity_id: {}",
            q.cypher
        );
    }

    #[test]
    fn build_cypher_targets_lunaris_graph_by_default() {
        let id = EntityId::from_name_and_type("Alice", "Person");
        let g = Graph::anchored(vec![(id, 1.0)], 2);
        let q = g.build_cypher();
        assert_eq!(q.graph, "lunaris_graph");
    }

    #[test]
    fn build_cypher_with_custom_graph_name() {
        // with_graph() lets tenant-isolated deployments scope each tenant to
        // its own AGE / Moon graph.
        let id = EntityId::from_name_and_type("Alice", "Person");
        let g = Graph::anchored(vec![(id, 1.0)], 2).with_graph("tenant_42_graph");
        let q = g.build_cypher();
        assert_eq!(q.graph, "tenant_42_graph");
    }

    #[test]
    fn graph_is_dyn_compatible() {
        // Compile-time proof Graph constructs as Box<dyn Retriever>; required
        // for the AndRetriever / FuseRrfRetriever composition path.
        let _: Box<dyn Retriever> = Box::new(Graph::anchored(no_seeds(), 2));
    }
}
