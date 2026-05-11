//! `vector_search` — typed `client.vector().search_raw(...)` wrapped with
//! `client.temporal().snapshot_at_packed(packed_hlc)` for AS_OF queries.
//!
//! RFC 0001 Wave 1C: the FT index consulted is the per-scope index
//! `ft_index_name(scope, index)` (e.g. `lunaris_acme:agent-1_chunks_idx`).
//! `decode_key` strips the per-scope FT index name prefix (not the bare `index`
//! name) before hex-decoding the ULID — the 16-byte ULID contract is preserved.
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

use lunaris_core::Scope;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::{Filter, VectorHit};

use crate::client::{MoonClient, moon_err};
use crate::keyspace::ft_index_name;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn vector_search(
    c: &MoonClient,
    scope: &Scope,
    index: &str,
    query: &[f32],
    k: usize,
    filter: Option<&Filter>,
    as_of: Option<Hlc>,
    rerank: bool,
) -> Result<Vec<VectorHit>, StorageError> {
    let typed = c.typed();

    // AS_OF deferred — Moon SDK `search_raw` does not yet expose an `as_of`
    // parameter; the SDK's `search_opts` does (`FT.SEARCH … AS_OF <ts>` clause)
    // but lacks the custom filter expression that Lunaris uses. Until either
    // SDK helper accepts both, AS_OF queries return current state. Phase 1.5
    // pre-pinned via `snapshot_at_packed(ts)` — that command takes 0 args
    // server-side and was rejected. Tracked as B-task for STORE-07.
    let _ = as_of;

    // RFC 0001 Wave 1C: route to the per-scope FT index.
    let per_scope_index = ft_index_name(scope, index);

    // Encode query embedding as little-endian f32 bytes per Moon FT convention.
    let mut qbytes = Vec::with_capacity(query.len() * 4);
    for f in query {
        qbytes.extend_from_slice(&f.to_le_bytes());
    }

    let filter_expr = filter.map(filter_to_moon).unwrap_or_else(|| "*".to_string());
    // Moon's FT.SEARCH KNN dialect requires the query to carry the full
    // `<filter>=>[KNN <k> @<field> $<param>]` form — Phase 1.5 used the SDK's
    // `search_raw` with bare filter, which Moon rejects with "invalid KNN
    // query syntax". The SDK's higher-level `search_opts` adds the KNN
    // wrapper but doesn't accept a filter expression. Compose the wrapper
    // here so the filter algebra still works. Live-measurement gap fix
    // 2026-04-21.
    let knn_query = format!("({filter_expr})=>[KNN {k} @vec $query]");
    let reply = typed
        .vector()
        .search_raw(&per_scope_index, &knn_query, &qbytes, k, rerank)
        .await
        .map_err(moon_err)?;
    parse_ft_search(reply, rerank, &per_scope_index)
}

/// Decode a Moon FT.SEARCH result key from `{ft_index}:{hex32}` to raw 16-byte ULID.
///
/// RFC 0001 Wave 1C: `ft_index` is now the per-scope index name
/// (`lunaris_{scope}_{kind}_idx`) rather than the bare `kind` name. The decode
/// contract is identical — strip the `<ft_index>:` prefix and hex-decode the
/// remainder to a 16-byte ULID. Any key that doesn't match the shape is dropped.
///
/// This preserves the existing FT.SEARCH key-decode contract (memory feedback:
/// "Moon FT.SEARCH key decode") — only the prefix length changes.
fn decode_key(key: &[u8], ft_index: &str) -> Option<Vec<u8>> {
    let prefix_len = ft_index.len() + 1; // +1 for the ':' separator
    if key.len() < prefix_len
        || !key.starts_with(ft_index.as_bytes())
        || key[ft_index.len()] != b':'
    {
        return None;
    }
    hex::decode(&key[prefix_len..]).ok().filter(|b| b.len() == 16)
}

// Removed `pack_hlc` (Gap 8 / live-measurement 2026-04-21): the
// `TEMPORAL.SNAPSHOT_AT` pre-pin path was deleted from vector/keyword/graph/kv
// after Moon proved it has no KV-AS_OF surface. If a real bi-temporal
// command surface lands upstream, restore the helper from git history rather
// than re-derive it.

fn filter_to_moon(f: &Filter) -> String {
    match f {
        Filter::Eq { field, value } => {
            if field == "source" {
                // source is a TAG field on the chunks FT index (PERF-MOON-01).
                // TAG syntax uses `{value}` braces. Special characters inside
                // the value must be backslash-escaped per RediSearch TAG rules.
                format!("@{field}:{{{}}}", ft_tag_escape(&json_to_moon_bare(value)))
            } else {
                format!("@{field}:{}", json_to_moon(value))
            }
        }
        Filter::StartsWith { field, prefix } => format!("@{field}:{prefix}*"),
        Filter::And(xs) => {
            format!("({})", xs.iter().map(filter_to_moon).collect::<Vec<_>>().join(" "))
        }
        Filter::Or(xs) => {
            format!("({})", xs.iter().map(filter_to_moon).collect::<Vec<_>>().join(" | "))
        }
        Filter::ValidTimeRange { after, before } => {
            // RediSearch numeric range on `@valid_time`. Inline string render —
            // no hlc_to_moon_score helper exists (verified 2026-04-22). None
            // maps to -inf / +inf sentinels. The Moon FT schema on the `chunks`
            // index declares `valid_time` NUMERIC (Plan 09.1-02 Task 2 landed
            // SchemaField::Numeric("valid_time") in ensure_indexes).
            let lo = after.map_or("-inf".to_string(), |h| h.wall_ms.to_string());
            let hi = before.map_or("+inf".to_string(), |h| h.wall_ms.to_string());
            format!("@valid_time:[{lo} {hi}]")
        }
    }
}

/// Return the bare string value WITHOUT quotes — TAG values must not be quoted.
fn json_to_moon_bare(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => format!("{v}"),
    }
}

/// Escape RediSearch TAG special characters with backslash.
/// Per RediSearch TAG rules: `,`, `.`, `{`, `}`, `\`, `:` must be escaped.
fn ft_tag_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        if matches!(ch, ',' | '.' | '{' | '}' | '\\' | ':') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn json_to_moon(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => format!("\"{s}\""),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => format!("\"{v}\""),
    }
}

fn parse_ft_search(
    v: redis::Value,
    rerank: bool,
    ft_index: &str,
) -> Result<Vec<VectorHit>, StorageError> {
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
        let raw_key = match id {
            redis::Value::BulkString(b) => b,
            redis::Value::SimpleString(s) => s.into_bytes(),
            _ => continue,
        };
        let id_bytes = match decode_key(&raw_key, ft_index) {
            Some(b) => b,
            None => continue,
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

    /// Helper: construct a per-scope FT index name for tests — mirrors what
    /// `atomic.rs` writes and what `vector_search` queries.
    fn test_ft_index(scope_str: &str, kind: &str) -> String {
        let scope = lunaris_core::Scope::new(scope_str).unwrap();
        ft_index_name(&scope, kind)
    }

    /// Plan 15-01 Task 3 — source is a TAG field; TAG syntax uses `{value}`
    /// braces. Dots inside the value must be backslash-escaped per RediSearch
    /// TAG rules.
    #[test]
    fn filter_eq_renders_source_as_tag() {
        let f = Filter::Eq { field: "source".into(), value: json!("notes.md") };
        assert_eq!(filter_to_moon(&f), "@source:{notes\\.md}");
    }

    #[test]
    fn filter_eq_renders_source_with_colon_as_tag() {
        let f = Filter::Eq { field: "source".into(), value: json!("helios:fs/test") };
        assert_eq!(filter_to_moon(&f), "@source:{helios\\:fs/test}");
    }

    #[test]
    fn filter_eq_renders_non_source_as_text() {
        let f = Filter::Eq { field: "kind".into(), value: json!("episode") };
        assert_eq!(filter_to_moon(&f), "@kind:\"episode\"");
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

    // Plan 09.1-02 Task 3 — ValidTimeRange rendering tests.
    #[test]
    fn filter_to_moon_valid_time_range_both_bounds() {
        let f = Filter::ValidTimeRange {
            after: Some(Hlc { wall_ms: 100, counter: 0, node_id: 0 }),
            before: Some(Hlc { wall_ms: 200, counter: 0, node_id: 0 }),
        };
        assert_eq!(filter_to_moon(&f), "@valid_time:[100 200]");
    }

    #[test]
    fn filter_to_moon_valid_time_range_only_after() {
        let f = Filter::ValidTimeRange {
            after: Some(Hlc { wall_ms: 100, counter: 0, node_id: 0 }),
            before: None,
        };
        assert_eq!(filter_to_moon(&f), "@valid_time:[100 +inf]");
    }

    #[test]
    fn filter_to_moon_valid_time_range_only_before() {
        let f = Filter::ValidTimeRange {
            after: None,
            before: Some(Hlc { wall_ms: 200, counter: 0, node_id: 0 }),
        };
        assert_eq!(filter_to_moon(&f), "@valid_time:[-inf 200]");
    }

    #[test]
    fn filter_to_moon_valid_time_range_both_none() {
        let f = Filter::ValidTimeRange { after: None, before: None };
        assert_eq!(filter_to_moon(&f), "@valid_time:[-inf +inf]");
    }

    #[test]
    fn parse_ft_search_empty_returns_empty_vec() {
        let ft_idx = test_ft_index("_dev_", "chunks");
        let reply = redis::Value::Array(vec![redis::Value::Int(0)]);
        let hits = parse_ft_search(reply, false, &ft_idx).unwrap();
        assert!(hits.is_empty());
    }

    /// RFC 0001 Wave 1C: FT.SEARCH keys are now `{ft_index_name(scope,kind)}:{hex32}`.
    /// `decode_key` must strip the per-scope prefix correctly to recover 16-byte ULID.
    #[test]
    fn parse_ft_search_decodes_per_scope_index_prefixed_hex_keys() {
        let scope = lunaris_core::Scope::new("acme.agent-1").unwrap();
        let ft_idx = ft_index_name(&scope, "chunks");
        let ulid_bytes: [u8; 16] =
            [1, 157, 184, 227, 97, 47, 203, 75, 14, 1, 202, 211, 92, 115, 47, 72];
        let hex = hex::encode(ulid_bytes);
        let key = format!("{ft_idx}:{hex}");
        let reply = redis::Value::Array(vec![
            redis::Value::Int(1),
            redis::Value::BulkString(key.into_bytes()),
            redis::Value::Array(vec![
                redis::Value::BulkString(b"__score".to_vec()),
                redis::Value::BulkString(b"0.91".to_vec()),
            ]),
        ]);
        let hits = parse_ft_search(reply, true, &ft_idx).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, ulid_bytes.to_vec());
        assert!((hits[0].score - 0.91).abs() < 1e-4);
        assert!(hits[0].rerank_applied);
    }

    #[test]
    fn parse_ft_search_drops_malformed_keys() {
        let ft_idx = test_ft_index("_dev_", "chunks");
        let reply = redis::Value::Array(vec![
            redis::Value::Int(1),
            redis::Value::BulkString(b"wrongprefix:abc".to_vec()),
            redis::Value::Array(vec![]),
        ]);
        let hits = parse_ft_search(reply, false, &ft_idx).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn decode_key_round_trip_with_scope() {
        let scope = lunaris_core::Scope::new("acme.agent-1").unwrap();
        let ft_idx = ft_index_name(&scope, "chunks");
        let ulid: [u8; 16] = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let key = format!("{ft_idx}:{}", hex::encode(ulid));
        assert_eq!(decode_key(key.as_bytes(), &ft_idx), Some(ulid.to_vec()));
        // A key from a different scope's index must not decode against scope_a's index.
        let scope_b = lunaris_core::Scope::new("other.agent-2").unwrap();
        let ft_idx_b = ft_index_name(&scope_b, "chunks");
        assert_eq!(decode_key(key.as_bytes(), &ft_idx_b), None);
    }

    #[test]
    fn decode_key_rejects_wrong_prefix() {
        let ft_idx = test_ft_index("_dev_", "chunks");
        assert_eq!(decode_key(b"facts:00", ft_idx.as_str()), None);
        assert_eq!(decode_key(format!("{ft_idx}:notHex!").as_bytes(), ft_idx.as_str()), None);
    }
}
