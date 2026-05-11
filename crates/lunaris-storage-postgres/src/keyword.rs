//! `keyword_search` — Postgres tsvector + `ts_rank_cd` BM25-style ranking.
//!
//! SQL pattern:
//!
//! ```sql
//! SELECT id, payload, ts_rank_cd(text_tsv, plainto_tsquery('english', $1)) AS score
//! FROM <table>
//! WHERE text_tsv @@ plainto_tsquery('english', $1)
//!   [AND <bi-temporal predicate when as_of is Some>]
//!   [AND <filter clause when filter is Some>]
//! ORDER BY score DESC
//! LIMIT $2;
//! ```
//!
//! `<table>` is whitelist-matched against `chunks|entities|facts|communities`
//! (T-02-02-01 / T-01-04-02 mitigation). The query string flows in via the bound
//! parameter `$1` to `plainto_tsquery` — never interpolated.
//!
//! ## Latency budget
//!
//! Each call wraps the sqlx query in a 500 ms `tokio::time::timeout`. A timeout
//! surfaces as `StorageError::Backend("keyword_search timeout (500ms)".to_string())`.
//! CLAUDE.md "design for failure" — the ingest hot path's recall p50 budget is
//! 25 ms (Moon) / 60 ms (Postgres) so 500 ms is a soft cap that flags pathological
//! queries without false positives under healthy load.
//!
//! ## Score normalization
//!
//! `ts_rank_cd` returns an unbounded positive `real`. We min-max normalize within
//! the call's result set per the [`KeywordPort`](lunaris_core::KeywordPort) contract:
//! `score = (raw - min) / (max - min)`. When `max == min` (single hit OR all tied)
//! every entry gets `score = 1.0` to preserve the tie semantics.

use std::time::Duration;

use lunaris_core::Scope;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::keyword::{KeywordHit, min_max_normalize};
use lunaris_core::storage::types::Filter;
use sqlx::{AssertSqlSafe, Row};
use tokio::time::timeout;

use crate::pool::{PgClient, sqlx_err};
use crate::schema::hlc_to_ts;

const KEYWORD_SEARCH_TIMEOUT: Duration = Duration::from_millis(500);

pub(crate) async fn keyword_search(
    c: &PgClient,
    // Wave 2.5A: scope accepted for API parity with RFC 0001 §3.4 amendment.
    // Postgres partitioning is enforced via RLS at the connection level (Wave 1B)
    // rather than per-query WHERE clauses, so _scope is unused here but the
    // parameter ensures the trait boundary is met across all backends.
    _scope: &Scope,
    index: &str,
    query: &str,
    k: usize,
    filter: Option<&Filter>,
    as_of: Option<Hlc>,
) -> Result<Vec<KeywordHit>, StorageError> {
    let table: &str = match index {
        "chunks" => "chunks",
        "entities" => "entities",
        "facts" => "facts",
        "communities" => "communities",
        other => {
            return Err(StorageError::Backend(format!(
                "unknown keyword index: {other} (valid: chunks|entities|facts|communities)"
            )));
        }
    };

    let mut where_parts: Vec<String> =
        vec!["text_tsv @@ plainto_tsquery('english', $1)".to_string()];

    if let Some(t) = as_of {
        let ts = hlc_to_ts(t).to_rfc3339();
        // SQL injection note: `ts` comes from a controlled HLC -> RFC3339 conversion;
        // not user-controlled text. Inlining keeps the SQL self-contained and lets
        // us bind only the user-supplied query as a parameter.
        where_parts.push(format!(
            "valid_from <= '{ts}' AND (valid_to IS NULL OR '{ts}' < valid_to) \
             AND sys_from <= '{ts}' AND (sys_to IS NULL OR '{ts}' < sys_to)"
        ));
    }

    if let Some(f) = filter {
        where_parts.push(filter_to_sql(f, "payload"));
    }

    let sql = format!(
        "SELECT id, payload, \
                ts_rank_cd(text_tsv, plainto_tsquery('english', $1)) AS score \
         FROM {table} \
         WHERE {where_clause} \
         ORDER BY score DESC \
         LIMIT {k}",
        where_clause = where_parts.join(" AND "),
    );

    let fut = sqlx::query(AssertSqlSafe(sql)).bind(query).fetch_all(&c.pool);
    let rows = match timeout(KEYWORD_SEARCH_TIMEOUT, fut).await {
        Ok(res) => res.map_err(sqlx_err)?,
        Err(_) => {
            return Err(StorageError::Backend(format!(
                "keyword_search timeout ({}ms)",
                KEYWORD_SEARCH_TIMEOUT.as_millis()
            )));
        }
    };

    // Collect raw scores first so min-max can normalize per-call.
    let mut staged: Vec<(Vec<u8>, serde_json::Value, f32)> = Vec::with_capacity(rows.len());
    for r in rows {
        let id: Vec<u8> = r.try_get("id").unwrap_or_default();
        let payload: serde_json::Value = r.try_get("payload").unwrap_or(serde_json::Value::Null);
        // ts_rank_cd returns `real` (f32). Use try_get::<f32, _> with f64 fallback.
        let raw_score: f32 = r
            .try_get::<f32, _>("score")
            .or_else(|_| r.try_get::<f64, _>("score").map(|v| v as f32))
            .unwrap_or(0.0);
        staged.push((id, payload, raw_score));
    }

    let raw_scores: Vec<f32> = staged.iter().map(|(_, _, r)| *r).collect();
    let normalized = min_max_normalize(&raw_scores);

    Ok(staged
        .into_iter()
        .zip(normalized)
        .map(|((id, payload, raw), score)| KeywordHit {
            id,
            score,
            raw_score: raw,
            metadata: payload,
        })
        .collect())
}

/// Translate a `Filter` tree to a parenthesised SQL boolean expression on the
/// `payload` JSONB column. Mirrors `vector::filter_to_sql` byte-for-byte —
/// re-implemented here (28 LOC) instead of extracting to a shared `sql_common.rs`
/// module per the file-extraction rule (extract when duplication > 30 LOC).
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

    // Plan 09.1-02 Task 4 — ValidTimeRange rendering tests (byte-identical
    // output to vector.rs::filter_to_sql).
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
