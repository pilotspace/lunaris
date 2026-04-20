//! `vector_search` — typed `client.vector().search_raw(...)` wrapped with
//! `client.temporal().snapshot_at_packed(packed_hlc)` for AS_OF queries.
//!
//! Phase 1.5 retrofit (STORE-09): all RESP commands here are dispatched through the
//! typed `moon-client` SDK. The Lunaris-shaped FT.SEARCH wire format (custom filter
//! expression + raw query bytes + RERANK flag) is exposed by `VectorClient::search_raw`
//! upstream so we keep both Lunaris's filter algebra AND the bi-temporal `__score` /
//! `__metadata` parser in this module.
//!
//! ## AS_OF semantics
//!
//! When `as_of = Some(t)`, we issue `client.temporal().snapshot_at_packed(packed_hlc)`
//! on the same connection BEFORE `FT.SEARCH` so the search reads the snapshot. After
//! the search we call `client.temporal().release_snapshot()` to release the pin back
//! to live mode. The `MultiplexedConnection` may multiplex this connection across
//! other tasks, so failure to release would pin a stale view for them — `release` runs
//! after the search even on FT.SEARCH errors (best effort).
//!
//! ## Filter algebra
//!
//! Phase 1 supports the v0 algebra (`Eq`, `StartsWith`, `And`, `Or`) translated into
//! Moon's FT query DSL: `@field:value`, `@field:prefix*`, space-joined for AND, `|`
//! for OR. Backends that don't support `FT.*` (e.g., Postgres) translate the same
//! algebra into `WHERE ...`.

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::{Filter, VectorHit};

use crate::client::{MoonClient, moon_err};

pub(crate) async fn vector_search(
    c: &MoonClient,
    index: &str,
    query: &[f32],
    k: usize,
    filter: Option<&Filter>,
    as_of: Option<Hlc>,
    rerank: bool,
) -> Result<Vec<VectorHit>, StorageError> {
    let typed = c.typed();

    // Pin the connection to a snapshot if AS_OF was requested.
    if let Some(t) = as_of {
        let pinned = pack_hlc(t);
        typed.temporal().snapshot_at_packed(pinned).await.map_err(moon_err)?;
    }

    // Encode query embedding as little-endian f32 bytes per Moon FT convention.
    let mut qbytes = Vec::with_capacity(query.len() * 4);
    for f in query {
        qbytes.extend_from_slice(&f.to_le_bytes());
    }

    let filter_expr = filter.map(filter_to_moon).unwrap_or_else(|| "*".to_string());

    let search_result = typed.vector().search_raw(index, &filter_expr, &qbytes, k, rerank).await;

    // Always release the snapshot if we pinned one (even on FT.SEARCH error).
    if as_of.is_some() {
        let _ = typed.temporal().release_snapshot().await;
    }

    let reply = search_result.map_err(moon_err)?;
    parse_ft_search(reply, rerank)
}

/// Pack an `Hlc` into the 96-bit value Moon expects for `TEMPORAL.SNAPSHOT_AT`.
///
/// Layout: `(wall_ms as u128) << 32 | (counter as u128)`. Node id is folded out (a
/// snapshot at time `t` is shared across nodes). Returned as `u128` — Moon parses the
/// stringified value via `BIGNUM`.
#[inline]
pub(crate) fn pack_hlc(t: Hlc) -> u128 {
    ((t.wall_ms as u128) << 32) | (t.counter as u128)
}

fn filter_to_moon(f: &Filter) -> String {
    match f {
        Filter::Eq { field, value } => format!("@{field}:{}", json_to_moon(value)),
        Filter::StartsWith { field, prefix } => format!("@{field}:{prefix}*"),
        Filter::And(xs) => {
            format!("({})", xs.iter().map(filter_to_moon).collect::<Vec<_>>().join(" "))
        }
        Filter::Or(xs) => {
            format!("({})", xs.iter().map(filter_to_moon).collect::<Vec<_>>().join(" | "))
        }
    }
}

fn json_to_moon(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => format!("\"{s}\""),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => format!("\"{v}\""),
    }
}

fn parse_ft_search(v: redis::Value, rerank: bool) -> Result<Vec<VectorHit>, StorageError> {
    // FT.SEARCH reply: [count, id1, [k1,v1,...], id2, [k1,v1,...], ...]
    let arr = match v {
        redis::Value::Array(a) => a,
        other => {
            return Err(StorageError::Backend(format!("FT.SEARCH unexpected reply: {other:?}")));
        }
    };
    let mut hits = Vec::new();
    let mut iter = arr.into_iter();
    let _count = iter.next();
    while let (Some(id), Some(fields)) = (iter.next(), iter.next()) {
        let id_bytes = match id {
            redis::Value::BulkString(b) => b,
            redis::Value::SimpleString(s) => s.into_bytes(),
            _ => continue,
        };
        let mut score = 0.0f32;
        let mut metadata = serde_json::Value::Null;
        if let redis::Value::Array(kv) = fields {
            let mut it = kv.into_iter();
            while let (Some(k), Some(val)) = (it.next(), it.next()) {
                let key = match k {
                    redis::Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
                    redis::Value::SimpleString(s) => s,
                    _ => continue,
                };
                match (key.as_str(), val) {
                    ("__score", redis::Value::BulkString(b)) => {
                        score = std::str::from_utf8(&b)
                            .ok()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0.0);
                    }
                    ("__metadata", redis::Value::BulkString(b)) => {
                        metadata = serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null);
                    }
                    _ => {}
                }
            }
        }
        hits.push(VectorHit { id: id_bytes, score, rerank_applied: rerank, metadata });
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::storage::types::Filter;
    use serde_json::json;

    #[test]
    fn filter_eq_renders() {
        let f = Filter::Eq { field: "source".into(), value: json!("notes.md") };
        assert_eq!(filter_to_moon(&f), "@source:\"notes.md\"");
    }

    #[test]
    fn filter_starts_with_renders() {
        let f = Filter::StartsWith { field: "source".into(), prefix: "helios:fs/".into() };
        assert_eq!(filter_to_moon(&f), "@source:helios:fs/*");
    }

    #[test]
    fn filter_and_or_combine() {
        let f = Filter::And(vec![
            Filter::Eq { field: "kind".into(), value: json!("episode") },
            Filter::Or(vec![
                Filter::Eq { field: "lang".into(), value: json!("en") },
                Filter::Eq { field: "lang".into(), value: json!("fr") },
            ]),
        ]);
        let s = filter_to_moon(&f);
        assert!(s.starts_with('('), "AND wraps in parens, got {s}");
        assert!(s.contains("@kind:\"episode\""));
        assert!(s.contains(" | "), "OR uses pipe, got {s}");
    }

    #[test]
    fn pack_hlc_round_trips_components() {
        let t = Hlc { wall_ms: 0xDEAD_BEEFu64, counter: 0xCAFEu32, node_id: 1 };
        let n = pack_hlc(t);
        assert_eq!((n >> 32) as u64, 0xDEAD_BEEFu64);
        assert_eq!((n & 0xFFFF_FFFF) as u32, 0xCAFEu32);
    }

    #[test]
    fn parse_ft_search_empty_returns_empty_vec() {
        let reply = redis::Value::Array(vec![redis::Value::Int(0)]);
        let hits = parse_ft_search(reply, false).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn parse_ft_search_two_hits_with_score() {
        let reply = redis::Value::Array(vec![
            redis::Value::Int(2),
            redis::Value::BulkString(b"doc1".to_vec()),
            redis::Value::Array(vec![
                redis::Value::BulkString(b"__score".to_vec()),
                redis::Value::BulkString(b"0.91".to_vec()),
            ]),
            redis::Value::BulkString(b"doc2".to_vec()),
            redis::Value::Array(vec![
                redis::Value::BulkString(b"__score".to_vec()),
                redis::Value::BulkString(b"0.55".to_vec()),
            ]),
        ]);
        let hits = parse_ft_search(reply, true).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, b"doc1".to_vec());
        assert!((hits[0].score - 0.91).abs() < 1e-4);
        assert!(hits[0].rerank_applied);
        assert_eq!(hits[1].id, b"doc2".to_vec());
    }
}
