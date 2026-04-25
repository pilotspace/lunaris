//! `vector_search` — pgvector cosine distance + bi-temporal predicate.
//!
//! SQL pattern:
//!
//! ```sql
//! SELECT id, payload, embedding <=> $1::vector AS distance
//! FROM <table>
//! WHERE embedding IS NOT NULL
//!   [AND <bi-temporal predicate when as_of is Some>]
//!   [AND <filter clause when filter is Some>]
//! ORDER BY distance ASC
//! LIMIT <k>;
//! ```
//!
//! `<table>` is whitelist-matched against `chunks|entities|facts|communities`
//! (T-01-04-02 mitigation).
//!
//! `rerank` is a hint; pgvector has no native cross-encoder, so we always emit
//! `rerank_applied = false`. Phase 2 may shell out to a candle reranker downstream.

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::{Filter, VectorHit};
use sqlx::{AssertSqlSafe, Row};

use crate::pool::{PgClient, sqlx_err};
use crate::schema::hlc_to_ts;

pub(crate) async fn vector_search(
    c: &PgClient,
    index: &str,
    query: &[f32],
    k: usize,
    filter: Option<&Filter>,
    as_of: Option<Hlc>,
    _rerank: bool,
) -> Result<Vec<VectorHit>, StorageError> {
    let (table, embedding_col): (&str, &str) = match index {
        "chunks" => ("chunks", "embedding"),
        "entities" => ("entities", "embedding"),
        "facts" => ("facts", "embedding"),
        "communities" => ("communities", "summary_embedding"),
        other => {
            return Err(StorageError::Backend(format!(
                "unknown vector index: {other} (valid: chunks|entities|facts|communities)"
            )));
        }
    };

    let qlit = vec_to_literal(query);

    let mut where_parts: Vec<String> = vec![format!("{embedding_col} IS NOT NULL")];

    if let Some(t) = as_of {
        let ts = hlc_to_ts(t).to_rfc3339();
        // SQL injection note: `ts` comes from a controlled HLC -> RFC3339 conversion;
        // not user-controlled text. Inlining keeps the SQL self-contained.
        where_parts.push(format!(
            "valid_from <= '{ts}' AND (valid_to IS NULL OR '{ts}' < valid_to) \
             AND sys_from <= '{ts}' AND (sys_to IS NULL OR '{ts}' < sys_to)"
        ));
    }

    if let Some(f) = filter {
        where_parts.push(filter_to_sql(f, "payload"));
    }

    let sql = format!(
        "SELECT id, payload, {embedding_col} <=> $1::vector AS distance \
         FROM {table} \
         WHERE {where_clause} \
         ORDER BY distance ASC \
         LIMIT {k}",
        where_clause = where_parts.join(" AND "),
    );

    let rows =
        sqlx::query(AssertSqlSafe(sql)).bind(qlit).fetch_all(&c.pool).await.map_err(sqlx_err)?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let id: Vec<u8> = r.try_get("id").unwrap_or_default();
            let payload: serde_json::Value =
                r.try_get("payload").unwrap_or(serde_json::Value::Null);
            let distance: f64 = r.try_get("distance").unwrap_or(1.0);
            VectorHit {
                id,
                // cosine distance ∈ [0,2] -> similarity ∈ [-1,1]; for v0 emit `1 - distance`.
                score: 1.0 - distance as f32,
                rerank_applied: false,
                metadata: payload,
            }
        })
        .collect())
}

/// Translate a `Filter` tree to a parenthesised SQL boolean expression on the
/// `payload` JSONB column.
///
/// T-01-04-01: field names come from caller code; v0 trusts callers to only pass
/// `^[a-zA-Z_][a-zA-Z0-9_]*$`. Phase 4 OPS-04 audit will validate at the trait.
/// String values are escaped (`'` -> `''`).
fn filter_to_sql(f: &Filter, payload_col: &str) -> String {
    match f {
        Filter::Eq { field, value } => match value {
            serde_json::Value::String(s) => {
                format!("{payload_col}->>'{field}' = '{}'", s.replace('\'', "''"))
            }
            other => {
                let raw = other.to_string();
                format!("{payload_col}->>'{field}' = '{}'", raw.replace('\'', "''"))
            }
        },
        Filter::StartsWith { field, prefix } => {
            format!("{payload_col}->>'{field}' LIKE '{}%'", prefix.replace('\'', "''"))
        }
        Filter::And(xs) => {
            let parts: Vec<String> = xs.iter().map(|x| filter_to_sql(x, payload_col)).collect();
            format!("({})", parts.join(" AND "))
        }
        Filter::Or(xs) => {
            let parts: Vec<String> = xs.iter().map(|x| filter_to_sql(x, payload_col)).collect();
            format!("({})", parts.join(" OR "))
        }
        Filter::ValidTimeRange { after, before } => {
            // Bi-temporal valid_time range. Uses the chunks table's
            // valid_from column (exists since Phase 4 schema; confirmed
            // 2026-04-22). Format timestamps via the existing hlc_to_ts
            // helper at crates/lunaris-storage-postgres/src/schema.rs
            // so rendering stays in lock-step with the as_of snapshot path
            // (which uses the same helper above at the `as_of` render).
            //
            // Both-None degrades to TRUE — a predicate no-op that composes
            // cleanly under AND with other filters.
            //
            // SQL injection note (T-09-1-02-05): hlc_to_ts(..).to_rfc3339()
            // emits a fixed grammar (digits / `-` / `T` / `:` / `.` / `Z`
            // / `+`) — structurally cannot carry a single-quote, semicolon,
            // or backslash. No injection surface.
            let mut parts = Vec::new();
            if let Some(a) = after {
                parts.push(format!("valid_from >= '{}'", hlc_to_ts(*a).to_rfc3339()));
            }
            if let Some(b) = before {
                parts.push(format!("valid_from < '{}'", hlc_to_ts(*b).to_rfc3339()));
            }
            if parts.is_empty() { "TRUE".to_string() } else { format!("({})", parts.join(" AND ")) }
        }
    }
}

/// Format a `&[f32]` as the pgvector literal `'[v0,v1,...]'`.
fn vec_to_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    let mut first = true;
    for f in v {
        if !first {
            s.push(',');
        }
        first = false;
        use std::fmt::Write as _;
        let _ = write!(s, "{f}");
    }
    s.push(']');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filter_eq_string_escapes_quotes() {
        let f = Filter::Eq { field: "source".into(), value: json!("alice's") };
        assert_eq!(filter_to_sql(&f, "payload"), "payload->>'source' = 'alice''s'");
    }

    #[test]
    fn filter_starts_with() {
        let f = Filter::StartsWith { field: "source".into(), prefix: "helios:fs/".into() };
        assert_eq!(filter_to_sql(&f, "payload"), "payload->>'source' LIKE 'helios:fs/%'");
    }

    #[test]
    fn filter_and_or_nest() {
        let f = Filter::And(vec![
            Filter::Eq { field: "kind".into(), value: json!("note") },
            Filter::Or(vec![
                Filter::Eq { field: "tag".into(), value: json!("a") },
                Filter::Eq { field: "tag".into(), value: json!("b") },
            ]),
        ]);
        let sql = filter_to_sql(&f, "payload");
        assert!(sql.starts_with("(payload->>'kind' = 'note' AND ("));
        assert!(sql.contains("payload->>'tag' = 'a' OR payload->>'tag' = 'b'"));
    }

    // Plan 09.1-02 Task 4 — ValidTimeRange rendering tests.
    #[test]
    fn filter_to_sql_valid_time_range_both_bounds() {
        let a = Hlc { wall_ms: 100, counter: 0, node_id: 0 };
        let b = Hlc { wall_ms: 200, counter: 0, node_id: 0 };
        let f = Filter::ValidTimeRange { after: Some(a), before: Some(b) };
        let sql = filter_to_sql(&f, "payload");
        let expected = format!(
            "(valid_from >= '{}' AND valid_from < '{}')",
            hlc_to_ts(a).to_rfc3339(),
            hlc_to_ts(b).to_rfc3339()
        );
        assert_eq!(sql, expected);
    }

    #[test]
    fn filter_to_sql_valid_time_range_only_after() {
        let a = Hlc { wall_ms: 100, counter: 0, node_id: 0 };
        let f = Filter::ValidTimeRange { after: Some(a), before: None };
        let sql = filter_to_sql(&f, "payload");
        let expected = format!("(valid_from >= '{}')", hlc_to_ts(a).to_rfc3339());
        assert_eq!(sql, expected);
    }

    #[test]
    fn filter_to_sql_valid_time_range_only_before() {
        let b = Hlc { wall_ms: 200, counter: 0, node_id: 0 };
        let f = Filter::ValidTimeRange { after: None, before: Some(b) };
        let sql = filter_to_sql(&f, "payload");
        let expected = format!("(valid_from < '{}')", hlc_to_ts(b).to_rfc3339());
        assert_eq!(sql, expected);
    }

    #[test]
    fn filter_to_sql_valid_time_range_both_none() {
        let f = Filter::ValidTimeRange { after: None, before: None };
        assert_eq!(filter_to_sql(&f, "payload"), "TRUE".to_string());
    }
}
