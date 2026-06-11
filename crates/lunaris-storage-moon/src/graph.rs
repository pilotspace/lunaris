//! `graph_traverse` — typed `client.graph().query_with_params(...)` / `query_raw`
//! for latest reads, and raw `GRAPH.QUERY ... VALID_AT <ms>` for AS_OF reads.
//!
//! TODO(v0.4): emit optional `path_length` + `edge_weight_product` columns to
//! feed `Graph::anchored`'s scoring formula
//! `(edge_weight_product / (1.0 + path_length)) * anchor_confidence`. Requires
//! lifting the operator's Cypher template to `MATCH p = (n)-[*1..N]-(m)` and
//! verifying Moon GRAPH.QUERY supports `length(p)` / `reduce(...)` over
//! variable-length paths. See `docs/v0.3-known-debt.md` § "Graph scoring".
//!
//! RFC 0001 Wave 1C: `GRAPH.QUERY` is routed to `graph_key(scope)` —
//! `lunaris_{scope}_graph` — so each scope has its own graph. The per-scope graph
//! is created lazily on first write (see `MoonStorage::ensure_scope`).
//!
//! Phase 1.5 retrofit (STORE-09): all RESP commands here go through the typed
//! `moon-client` SDK. The Lunaris-shaped GRAPH.QUERY wire format with `--params <json>`
//! is exposed by `GraphClient::query_with_params` upstream so we keep our own
//! bi-temporal-aware parser in this module.
//!
//! Moon's `GRAPH.QUERY <graph> "<cypher>"` reply layout (per Moon RESP docs):
//!
//! ```text
//! [
//!   [<header1>, <header2>, ...],   ; column names
//!   [                                ; rows array
//!     [<cell11>, <cell12>, ...],
//!     [<cell21>, <cell22>, ...],
//!   ],
//!   [<stat1>, <stat2>, ...],       ; statistics — currently ignored
//! ]
//! ```
//!
//! We extract `headers + rows`; cells are coerced to `serde_json::Value` (preserving
//! Int / String / Array / Nil shapes; `Boolean` mapped to `Bool`; everything else
//! falls back to its Debug-formatted form so callers can still see what came back).

use lunaris_core::Scope;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::{CypherQuery, GraphDecay, GraphResult};

use crate::client::{MoonClient, moon_err, redis_err};
use crate::keyspace::graph_key;

pub(crate) async fn graph_traverse(
    c: &MoonClient,
    scope: &Scope,
    query: &CypherQuery,
    as_of: Option<Hlc>,
) -> Result<GraphResult, StorageError> {
    let typed = c.typed();

    // RFC 0001 Wave 1C: route to per-scope graph key.
    let scope_graph = graph_key(scope);

    let result: Result<redis::Value, StorageError> = if let Some(ts) = as_of {
        query_raw_valid_at(c, scope_graph.as_str(), query, ts).await
    } else if !query.params.is_empty() {
        let params_json = serde_json::to_string(&query.params)?;
        typed
            .graph()
            .query_with_params(scope_graph.as_str(), query.cypher.as_str(), &params_json)
            .await
            .map_err(moon_err)
    } else {
        typed.graph().query_raw(scope_graph.as_str(), query.cypher.as_str()).await.map_err(moon_err)
    };

    parse_graph_reply(result?)
}

async fn query_raw_valid_at(
    c: &MoonClient,
    graph: &str,
    query: &CypherQuery,
    as_of: Hlc,
) -> Result<redis::Value, StorageError> {
    query_raw_with_clauses(c, graph, query, Some(as_of), None).await
}

/// ADD task `graph-decay-recency` (contract v1): recency-decay traversal via
/// Moon's `GRAPH.QUERY ... --decay <λ> [--time-weight <w>]` read-path clause.
/// Composes with `--params` and `VALID_AT` on a single command line. Moon
/// rejects the flag on write Cypher server-side — that surfaces here as a
/// `StorageError::Backend` passthrough (Lunaris does not pre-parse Cypher).
pub(crate) async fn graph_traverse_decayed(
    c: &MoonClient,
    scope: &Scope,
    query: &CypherQuery,
    as_of: Option<Hlc>,
    decay: &GraphDecay,
) -> Result<GraphResult, StorageError> {
    let scope_graph = graph_key(scope);
    let result = query_raw_with_clauses(c, scope_graph.as_str(), query, as_of, Some(decay)).await;
    parse_graph_reply(result?)
}

/// Shared raw-RESP `GRAPH.QUERY` builder. Clause order mirrors Moon's read
/// path: `<graph> "<cypher>" [--params <json>] [--decay <λ> [--time-weight <w>]]
/// [VALID_AT <ms>]`.
async fn query_raw_with_clauses(
    c: &MoonClient,
    graph: &str,
    query: &CypherQuery,
    as_of: Option<Hlc>,
    decay: Option<&GraphDecay>,
) -> Result<redis::Value, StorageError> {
    let mut typed = c.typed();
    let raw_conn = typed.inner_mut();
    let mut cmd = redis::cmd("GRAPH.QUERY");
    cmd.arg(graph).arg(query.cypher.as_str());
    if !query.params.is_empty() {
        let params_json = serde_json::to_string(&query.params)?;
        cmd.arg("--params").arg(params_json);
    }
    if let Some(d) = decay {
        cmd.arg("--decay").arg(d.lambda());
        if let Some(w) = d.time_weight() {
            cmd.arg("--time-weight").arg(w);
        }
    }
    if let Some(ts) = as_of {
        cmd.arg("VALID_AT").arg(ts.wall_ms as i64);
    }
    cmd.query_async(raw_conn).await.map_err(redis_err)
}

fn parse_graph_reply(v: redis::Value) -> Result<GraphResult, StorageError> {
    let arr = match v {
        redis::Value::Array(a) => a,
        other => {
            return Err(StorageError::Backend(format!("GRAPH.QUERY unexpected reply: {other:?}")));
        }
    };
    if arr.len() < 2 {
        // Empty / malformed reply — return an empty result (write-only Cypher returns
        // headers only; some Moon builds return just `OK` for those).
        return Ok(GraphResult { headers: vec![], rows: vec![] });
    }
    let mut iter = arr.into_iter();
    // SAFETY: the `arr.len() < 2` early-return above guarantees at
    // least two elements remain — header at index 0, rows at index 1.
    let header_v = iter.next().expect("guarded by arr.len() < 2 above");
    let rows_v = iter.next().expect("guarded by arr.len() < 2 above");

    let headers: Vec<String> = match header_v {
        redis::Value::Array(hs) => hs
            .into_iter()
            .filter_map(|h| match h {
                redis::Value::BulkString(b) => Some(String::from_utf8_lossy(&b).into_owned()),
                redis::Value::SimpleString(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => vec![],
    };

    let rows: Vec<Vec<serde_json::Value>> = match rows_v {
        redis::Value::Array(rs) => rs
            .into_iter()
            .map(|row| match row {
                redis::Value::Array(cells) => cells.into_iter().map(redis_to_json).collect(),
                other => vec![redis_to_json(other)],
            })
            .collect(),
        _ => vec![],
    };

    Ok(GraphResult { headers, rows })
}

fn redis_to_json(v: redis::Value) -> serde_json::Value {
    match v {
        redis::Value::Nil => serde_json::Value::Null,
        redis::Value::Int(n) => serde_json::Value::Number(n.into()),
        redis::Value::SimpleString(s) => serde_json::Value::String(s),
        redis::Value::BulkString(b) => {
            serde_json::Value::String(String::from_utf8_lossy(&b).into_owned())
        }
        redis::Value::Boolean(b) => serde_json::Value::Bool(b),
        redis::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(redis_to_json).collect())
        }
        other => serde_json::Value::String(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_reply_returns_empty_result() {
        // Single-element array (write-only Cypher).
        let v = redis::Value::Array(vec![redis::Value::SimpleString("OK".into())]);
        let r = parse_graph_reply(v).unwrap();
        assert!(r.headers.is_empty());
        assert!(r.rows.is_empty());
    }

    #[test]
    fn parse_two_column_two_row_reply() {
        let v = redis::Value::Array(vec![
            redis::Value::Array(vec![
                redis::Value::BulkString(b"name".to_vec()),
                redis::Value::BulkString(b"age".to_vec()),
            ]),
            redis::Value::Array(vec![
                redis::Value::Array(vec![
                    redis::Value::BulkString(b"alice".to_vec()),
                    redis::Value::Int(30),
                ]),
                redis::Value::Array(vec![
                    redis::Value::BulkString(b"bob".to_vec()),
                    redis::Value::Int(40),
                ]),
            ]),
            redis::Value::Array(vec![]), // stats
        ]);
        let r = parse_graph_reply(v).unwrap();
        assert_eq!(r.headers, vec!["name", "age"]);
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0][0], serde_json::json!("alice"));
        assert_eq!(r.rows[0][1], serde_json::json!(30));
        assert_eq!(r.rows[1][0], serde_json::json!("bob"));
    }

    /// Wave 4 Piece C: pin the N-column passthrough contract.
    ///
    /// `parse_graph_reply` is header-keyed (line 95-105 reads headers from
    /// the reply's first array element), so an arbitrary number of columns
    /// flows through unchanged. This test fixes that contract against
    /// regressions — when a future Cypher template (Moon-compatible variant
    /// of Wave 4 Piece A's `path_length` / `edge_weight_product` emission)
    /// returns six columns, the SDK layer MUST hand them off verbatim to the
    /// retrieval operator. The operator reads headers by name
    /// (`lunaris-retrieve::operators::graph`), so column order is irrelevant
    /// as long as the headers are preserved.
    ///
    /// Why this test exists: `tmp/wave4c-dialect-verification.md` confirmed
    /// that Moon's GRAPH dialect cannot emit `path_length` /
    /// `edge_weight_product` over a variable-length path today (parser
    /// rejects `MATCH p = (n)-[*1..N]-(m)` outside `shortestPath()` per
    /// `vendor/moon/src/graph/cypher/parser/pattern.rs:172-216`). But the
    /// Moon SDK layer should be ready the moment the operator switches to a
    /// backend-aware template (e.g. `shortestPath()`-based or
    /// post-traversal client-side accumulation). Pinning the contract here
    /// avoids a silent regression when that switch lands.
    #[test]
    fn parse_six_column_reply_with_path_metrics() {
        let v = redis::Value::Array(vec![
            // Headers — mirrors the Wave 4 Piece A column set.
            redis::Value::Array(vec![
                redis::Value::BulkString(b"id".to_vec()),
                redis::Value::BulkString(b"name".to_vec()),
                redis::Value::BulkString(b"type".to_vec()),
                redis::Value::BulkString(b"path_length".to_vec()),
                redis::Value::BulkString(b"edge_weight_product".to_vec()),
                redis::Value::BulkString(b"source_entity_id".to_vec()),
            ]),
            // Two rows: a 1-hop neighbour at weight 0.5, and a 2-hop
            // neighbour at weight 0.5 * 0.8 = 0.4. Both share the same
            // anchor entity. The 2-hop row carries a Nil `name` to confirm
            // null passthrough.
            redis::Value::Array(vec![
                redis::Value::Array(vec![
                    redis::Value::BulkString(b"BBB".to_vec()),
                    redis::Value::BulkString(b"Bravo".to_vec()),
                    redis::Value::BulkString(b"Person".to_vec()),
                    redis::Value::Int(1),
                    // Moon's typed reply uses bulk strings for floats; the
                    // operator reads by header name and coerces via
                    // `as_f64`/`parse`, so passthrough as a string is OK.
                    redis::Value::BulkString(b"0.5".to_vec()),
                    redis::Value::BulkString(b"AAA".to_vec()),
                ]),
                redis::Value::Array(vec![
                    redis::Value::BulkString(b"CCC".to_vec()),
                    redis::Value::Nil,
                    redis::Value::BulkString(b"Place".to_vec()),
                    redis::Value::Int(2),
                    redis::Value::BulkString(b"0.4".to_vec()),
                    redis::Value::BulkString(b"AAA".to_vec()),
                ]),
            ]),
            // Stats — ignored by parser.
            redis::Value::Array(vec![]),
        ]);
        let r = parse_graph_reply(v).expect("parse 6-column reply");

        assert_eq!(
            r.headers,
            vec!["id", "name", "type", "path_length", "edge_weight_product", "source_entity_id",],
            "headers must passthrough verbatim — operator reads by name",
        );
        assert_eq!(r.rows.len(), 2, "two rows expected");

        // Row 0 — 1-hop neighbour.
        assert_eq!(r.rows[0][0], serde_json::json!("BBB"));
        assert_eq!(r.rows[0][1], serde_json::json!("Bravo"));
        assert_eq!(r.rows[0][2], serde_json::json!("Person"));
        assert_eq!(r.rows[0][3], serde_json::json!(1));
        assert_eq!(r.rows[0][4], serde_json::json!("0.5"));
        assert_eq!(r.rows[0][5], serde_json::json!("AAA"));

        // Row 1 — 2-hop neighbour with Nil name passthrough.
        assert_eq!(r.rows[1][0], serde_json::json!("CCC"));
        assert_eq!(r.rows[1][1], serde_json::Value::Null);
        assert_eq!(r.rows[1][2], serde_json::json!("Place"));
        assert_eq!(r.rows[1][3], serde_json::json!(2));
        assert_eq!(r.rows[1][4], serde_json::json!("0.4"));
        assert_eq!(r.rows[1][5], serde_json::json!("AAA"));
    }

    #[test]
    fn redis_to_json_handles_nested_array() {
        let v = redis::Value::Array(vec![
            redis::Value::Int(1),
            redis::Value::BulkString(b"two".to_vec()),
            redis::Value::Nil,
        ]);
        let j = redis_to_json(v);
        assert_eq!(j, serde_json::json!([1, "two", null]));
    }
}
