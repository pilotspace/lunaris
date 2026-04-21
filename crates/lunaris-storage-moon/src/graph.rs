//! `graph_traverse` — typed `client.graph().query_with_params(...)` (or `query_raw`)
//! wrapped with `client.temporal().snapshot_at_packed(...)` for AS_OF queries.
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

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::{CypherQuery, GraphResult};

use crate::client::{MoonClient, moon_err};

pub(crate) async fn graph_traverse(
    c: &MoonClient,
    query: &CypherQuery,
    as_of: Option<Hlc>,
) -> Result<GraphResult, StorageError> {
    let typed = c.typed();

    // AS_OF deferred — Moon's `GRAPH.QUERY ... VALID_AT <ts>` clause is
    // documented but the SDK's `query_with_params` / `query_raw` do not
    // accept it as a parameter. Phase 1.5's `snapshot_at_packed(ts)`
    // pre-pin sent invalid args server-side. Until the SDK exposes a
    // VALID_AT helper (or we cmd-build inline), VALID_AT queries return
    // current state. Tracked as B-task for STORE-07.
    let _ = as_of;

    let result = if !query.params.is_empty() {
        let params_json = serde_json::to_string(&query.params)?;
        typed
            .graph()
            .query_with_params(query.graph.as_str(), query.cypher.as_str(), &params_json)
            .await
    } else {
        typed.graph().query_raw(query.graph.as_str(), query.cypher.as_str()).await
    };

    parse_graph_reply(result.map_err(moon_err)?)
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
    let header_v = iter.next().unwrap();
    let rows_v = iter.next().unwrap();

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
