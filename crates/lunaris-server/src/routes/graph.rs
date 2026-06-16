//! Memory Inspector (Phase 1) — root-anchored entity-graph neighborhood.
//!
//! `GET /v1/graph?root=<32-hex>&depth=<n>` (TASK `graph-endpoint` §3, FROZEN
//! @ v1): traverse the caller's scope graph from `root` out to `depth` hops via
//! `StoragePort::graph_traverse` and return the root-anchored NODE neighborhood
//! `{ root, depth, nodes:[{id,name,type}], truncated, graph_native:true }`.
//!
//! Nodes are deduped by id and EXCLUDE the root anchor (returned separately;
//! an undirected walk can revisit it). `truncated` is `true` when the traversal
//! returned `≥ DEFAULT_GRAPH_K` rows (the `LIMIT` was hit — more may exist).
//!
//! ## v1 scope boundary — nodes, not edges
//!
//! The anchored `CypherQuery` mirrors `Graph::anchored`'s Legacy template
//! (`MATCH (n)-[*1..N]-(m) WHERE n.id_hex = sid RETURN m.id_hex, name, type`),
//! which yields reachable nodes. The anchor filter MUST be a `WHERE` clause:
//! Moon silently ignores the inline-property form `(n {id_hex: sid})` (matches
//! every node, so `root`/`depth` stop constraining — live-confirmed in the
//! Memory Inspector UAT, 2026-06-16). Moon's `CypherDialect::Legacy` cannot
//! bind the per-hop edges of a variable-length path, so explicit edges are out
//! of v1 scope (documented follow-up). Phase-1 is Moon-native, so this is the
//! portable contract.
//!
//! ## Empty root → all nodes
//!
//! An absent/empty `root` lists every node in the scope graph (bare
//! `MATCH (n) … LIMIT k`, no anchor, `depth` ignored) so a reviewer can explore
//! the graph without first knowing an entity id.
//!
//! ## Design-for-failure
//!
//! The capability gate (501 `graph_unavailable`) fires BEFORE any storage call;
//! `root`/`depth` validation (400) fires BEFORE the traversal. `depth` only
//! ever enters the cypher as a validated literal (`1..=MAX_GRAPH_HOPS`); `root`
//! rides only in the `$ids` param — neither is string-interpolated from raw
//! user input. The route is strictly read-only (no `WriteOp`).

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use lunaris_core::LunarisError;
use lunaris_core::storage::types::CypherQuery;
use lunaris_retrieve::{
    DEFAULT_GRAPH_HOPS, DEFAULT_GRAPH_K, EntityId, LUNARIS_GRAPH_NAME, MAX_GRAPH_HOPS,
};

use crate::dto::GraphQuery;
use crate::middleware::auth::AuthClaims;
use crate::middleware::error::map_error;
use crate::state::AppState;

/// Handler for `GET /v1/graph?root=&depth=`.
///
/// Resolution order (per the frozen §3 + the Inspector-UAT change): (1) capability
/// gate → 501; (2) mode select on `root` — absent/empty ⇒ all-nodes scan,
/// present-but-not-hex ⇒ 400 `invalid_root`, valid ⇒ anchored; (3) anchored only:
/// validate `depth` → 400 `invalid_depth`; (4) `graph_traverse` → map rows |
/// `NotSupported`=501 | `Err`=500.
pub async fn graph_handler(
    State(state): State<AppState>,
    Extension(claims): Extension<AuthClaims>,
    Query(q): Query<GraphQuery>,
) -> Response {
    let scope = &claims.scope;

    // (1) Capability gate — graph mode requires native graph support or the
    //     runtime pipeline toggle. Mirrors the recall gate; fires BEFORE any
    //     storage traversal.
    let caps = state.lunaris.storage().capabilities();
    let graph_runtime_on = state.lunaris.graph_pipeline().is_enabled();
    if !caps.graph_native && !graph_runtime_on {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "error": "graph_unavailable",
                "message": "graph traversal requires capabilities().graph_native or GraphPipeline::enable()",
            })),
        )
            .into_response();
    }

    // (2) Mode selection on `root`. An ABSENT or empty/whitespace `root` lists
    //     ALL nodes in the scope graph — the "explore" entry point, since entity
    //     ids are not browsable via `/v1/browse` (they are graph-native). A
    //     non-empty `root` that is not 32-char hex is a 400; a valid `root`
    //     anchors a neighborhood traversal.
    let root_trimmed = q.root.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let (cypher, params, root_resp, depth_resp) = match root_trimmed {
        None => {
            // ALL-NODES mode — bare scan, no anchor, no hop bound. `depth` is
            // irrelevant here and is echoed as null.
            let cypher = "MATCH (n) RETURN n.id_hex AS id, n.name AS name, n.type AS type LIMIT $k"
                .to_string();
            let mut params = serde_json::Map::new();
            params.insert("k".into(), Value::from(DEFAULT_GRAPH_K));
            (cypher, params, Value::Null, Value::Null)
        }
        Some(root_str) => {
            // ANCHORED mode. Normalize to lowercase via EntityId round-trip so an
            // uppercase input still resolves + echoes canonically.
            let root_id = match EntityId::from_hex(root_str) {
                Some(id) => id,
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": "invalid_root",
                            "message": "root must be a 32-character lowercase-hex entity id",
                        })),
                    )
                        .into_response();
                }
            };
            let root_hex = root_id.to_string();

            // Validate depth (default DEFAULT_GRAPH_HOPS). The inspector REJECTS
            // an out-of-range depth rather than silently clamping like the operator.
            let depth = q.depth.unwrap_or(DEFAULT_GRAPH_HOPS);
            if depth == 0 || depth > MAX_GRAPH_HOPS {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "invalid_depth",
                        "message": format!("depth must be in 1..={MAX_GRAPH_HOPS}"),
                    })),
                )
                    .into_response();
            }

            // `depth` is a validated literal (openCypher needs literal path
            // bounds); `root` rides only in $ids. The anchor MUST be filtered via
            // a WHERE clause — Moon SILENTLY IGNORES the inline-property form
            // `(n {id_hex: sid})` (live-confirmed), matching every node as an
            // anchor so the traversal returns the whole connected component and
            // neither `root` nor `depth` constrains.
            let cypher = format!(
                "UNWIND $ids AS sid MATCH (n)-[*1..{depth}]-(m) WHERE n.id_hex = sid \
                 RETURN m.id_hex AS id, m.name AS name, m.type AS type LIMIT $k"
            );
            let mut params = serde_json::Map::new();
            params.insert("ids".into(), Value::Array(vec![Value::String(root_hex.clone())]));
            params.insert("k".into(), Value::from(DEFAULT_GRAPH_K));
            (cypher, params, Value::String(root_hex), Value::from(depth))
        }
    };

    // (3) Traverse + project. In all-nodes mode there is no anchor to exclude, so
    //     pass "" (no node has an empty id) and nothing is dropped.
    let root_excl = root_resp.as_str().unwrap_or("").to_string();
    let query = CypherQuery { graph: LUNARIS_GRAPH_NAME.to_string(), cypher, params };

    match state.lunaris.storage().graph_traverse(scope, &query, None).await {
        Ok(result) => {
            let truncated = result.rows.len() >= DEFAULT_GRAPH_K;
            let nodes = map_nodes(&result, &root_excl);
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "root": root_resp,
                    "depth": depth_resp,
                    "nodes": nodes,
                    "truncated": truncated,
                    "graph_native": true,
                })),
            )
                .into_response()
        }
        Err(e) => map_error(LunarisError::Storage(e)),
    }
}

/// Project a [`GraphResult`](lunaris_core::storage::types::GraphResult) into the
/// `nodes` array: read `id`/`name`/`type` BY HEADER NAME (wire-additive), dedup
/// by id, and drop the root anchor.
fn map_nodes(result: &lunaris_core::storage::types::GraphResult, root_hex: &str) -> Vec<Value> {
    let col = |name: &str| result.headers.iter().position(|h| h == name);
    let (id_c, name_c, type_c) = (col("id"), col("name"), col("type"));

    let mut seen: HashSet<String> = HashSet::new();
    let mut nodes: Vec<Value> = Vec::new();
    for row in &result.rows {
        let cell = |c: Option<usize>| c.and_then(|i| row.get(i)).cloned().unwrap_or(Value::Null);
        let id = cell(id_c);
        // Skip rows without a usable id, the root anchor itself, and duplicates.
        let Some(id_str) = id.as_str() else { continue };
        if id_str == root_hex || !seen.insert(id_str.to_string()) {
            continue;
        }
        nodes.push(serde_json::json!({
            "id": id,
            "name": cell(name_c),
            "type": cell(type_c),
        }));
    }
    nodes
}
