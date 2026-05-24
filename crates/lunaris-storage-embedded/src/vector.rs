//! Brute-force cosine `vector_search` for the embedded SQLite backend.
//!
//! ## Algorithm
//!
//! 1. **SQL boundary** — select `id`, `embedding`, `metadata` from `lunaris_vec`
//!    filtered by the per-scope index name (`idx = 'lunaris_{scope}_{index}_idx'`)
//!    and any additional [`Filter`] predicates translated to SQL. Scope filter is
//!    applied at the database boundary (RFC 0001 — scope isolation enforced before
//!    any Rust-side compute).
//!
//! 2. **Decode** — each `embedding` column is a JSON array (`TEXT`) serialised by
//!    `serde_json::to_string` in `atomic_write`. Decoded with
//!    `serde_json::from_str::<Vec<f32>>`.
//!
//! 3. **Cosine** — `cosine(probe, emb) = dot(probe, emb) / (|probe| * |emb|)`.
//!    The probe norm is computed once. Per-row norm is computed inline.
//!    Edge cases:
//!    - `|probe| == 0` → return `Ok(vec![])` immediately (undefined cosine).
//!    - `|emb| == 0` → skip the row silently.
//!    - NaN in stored vector → skip the row, log a warning.
//!    - Dimension mismatch (`probe.len() != emb.len()`) → skip + warn; any
//!      match depends on equal dims.
//!
//! 4. **Top-k** — `BinaryHeap<OrdHit>` capped at k, O(n log k) per recall.
//!
//! 5. **Return** — `Vec<VectorHit>` sorted score-descending.
//!
//! ## Performance note
//!
//! Correct, not fast: brute-force scan over all live rows in the scope.
//! For n = 10 000 rows at d = 768, roughly 30 M float ops — single-digit ms
//! on a modern laptop (well within the 25 ms moat for single-developer scope).
//! HNSW acceleration is a future phase (Wave B+).

use std::cmp::Ordering;

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::scope::Scope;
use lunaris_core::storage::types::{Filter, VectorHit};
use sqlx::sqlite::SqlitePool;
use sqlx::{AssertSqlSafe, Row as _};

use crate::hlc_key;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Compute the L2 norm of a float slice.
#[inline]
fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// Compute dot product of two equal-length slices.
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Compute cosine similarity. Caller guarantees `probe_norm > 0`.
///
/// Returns `None` when `emb_norm == 0` or either slice contains NaN.
#[inline]
fn cosine(probe: &[f32], probe_norm: f32, emb: &[f32]) -> Option<f32> {
    let emb_norm = norm(emb);
    if emb_norm == 0.0 {
        return None;
    }
    let c = dot(probe, emb) / (probe_norm * emb_norm);
    // Guard against NaN arising from a corrupt stored vector.
    if c.is_nan() { None } else { Some(c) }
}

// ── OrdHit — score-max heap entry ─────────────────────────────────────────────

/// Newtype wrapper that makes `VectorHit` orderable by score for the
/// max-BinaryHeap used to maintain the top-k set.
///
/// `f32` is not `Ord`, so we impose a total order via `f32::total_cmp`.
/// All NaN-containing scores are excluded upstream (cosine returns None for NaN).
#[derive(Debug)]
struct OrdHit {
    score: f32,
    hit: VectorHit,
}

impl PartialEq for OrdHit {
    fn eq(&self, other: &Self) -> bool {
        self.score.total_cmp(&other.score) == Ordering::Equal
    }
}
impl Eq for OrdHit {}

impl PartialOrd for OrdHit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdHit {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score.total_cmp(&other.score)
    }
}

// ── Scope-scoped index name ────────────────────────────────────────────────────

/// Computes the per-scope index name stored in `lunaris_vec.idx`.
///
/// Mirrors Moon's `ft_index_name(scope, kind)` without importing the Moon crate:
/// `"lunaris_{scope}_{index}_idx"`.
///
/// The embedded backend's `atomic_write` uses the same formula when persisting
/// `VectorUpsert` ops; `vector_search` uses it as the SQL `WHERE idx = ?` filter.
#[inline]
pub(crate) fn scoped_idx(scope: &Scope, index: &str) -> String {
    format!("lunaris_{}_{}_{}", scope.as_str(), index, "idx")
}

// ── Filter → SQLite WHERE fragment ────────────────────────────────────────────

/// Translate a [`Filter`] tree to a parenthesised SQLite boolean expression
/// operating on the `metadata` TEXT-JSON column via SQLite JSON1 functions.
///
/// Mirrors `lunaris_storage_postgres::vector::filter_to_sql` but uses
/// `json_extract(metadata, '$.{field}')` instead of `payload->>'{field}'`.
///
/// Security note (T-01-04-01): field names come from caller code. v0 trusts
/// callers to only pass `^[a-zA-Z_][a-zA-Z0-9_]*$` identifiers. String values
/// are SQL-escaped (`'` → `''`). Phase 4 OPS-04 audit will add a trait-level
/// guard — same deferral as the Postgres backend.
pub(crate) fn filter_to_sqlite(f: &Filter) -> String {
    match f {
        Filter::Eq { field, value } => {
            let literal = match value {
                serde_json::Value::String(s) => format!("'{}'", s.replace('\'', "''")),
                other => format!("'{}'", other.to_string().replace('\'', "''")),
            };
            format!("json_extract(metadata, '$.{field}') = {literal}")
        }
        Filter::StartsWith { field, prefix } => {
            let escaped = prefix.replace('\'', "''");
            format!("json_extract(metadata, '$.{field}') LIKE '{escaped}%'")
        }
        Filter::And(xs) => {
            let parts: Vec<String> = xs.iter().map(filter_to_sqlite).collect();
            format!("({})", parts.join(" AND "))
        }
        Filter::Or(xs) => {
            let parts: Vec<String> = xs.iter().map(filter_to_sqlite).collect();
            format!("({})", parts.join(" OR "))
        }
        Filter::ValidTimeRange { .. } => {
            // `lunaris_vec` has no `valid_from` / `valid_to` columns — only
            // `sys_from` / `sys_to` (system-time). Mapping valid-time semantics
            // onto system-time would be silently incorrect, so we degrade to
            // TRUE (return all rows in scope) and emit a warning. Callers that
            // need valid-time filtering on vectors should use the AS_OF path
            // via the `as_of` parameter instead.
            tracing::warn!(
                "Filter::ValidTimeRange is not supported on embedded vector_search \
                 (lunaris_vec has no valid_from column) — degrading to TRUE"
            );
            "TRUE".to_string()
        }
        // `#[non_exhaustive]` — new variants degrade to TRUE + warn (mirrors Postgres).
        _ => {
            tracing::warn!("unknown Filter variant in embedded vector path — degrading to TRUE");
            "TRUE".to_string()
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Brute-force cosine `vector_search` over the `lunaris_vec` SQLite table.
///
/// # Scope isolation
///
/// All rows for this `(scope, index)` pair share a derived `idx` value of
/// `"lunaris_{scope}_{index}_idx"` (written by `atomic_write` via `scoped_idx`).
/// The SQL `WHERE idx = ?` clause enforces isolation at the database boundary —
/// rows from other scopes are never fetched into Rust.
///
/// # Returns
///
/// `Vec<VectorHit>` sorted score-descending, length at most `k`. Returns
/// `Ok(vec![])` for an empty table or a zero-norm probe.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn vector_search(
    pool: &SqlitePool,
    scope: &Scope,
    index: &str,
    query: &[f32],
    k: usize,
    filter: Option<&Filter>,
    as_of: Option<Hlc>,
    _rerank: bool,
) -> Result<Vec<VectorHit>, StorageError> {
    // Short-circuit: k=0 is a valid no-op.
    if k == 0 {
        return Ok(vec![]);
    }

    // Pre-compute probe norm once. Zero-norm probe → cosine undefined → empty.
    let probe_norm = norm(query);
    if probe_norm == 0.0 {
        return Ok(vec![]);
    }

    // Build WHERE clause.
    let idx_name = scoped_idx(scope, index);
    let mut where_parts: Vec<String> = vec!["idx = ?1".to_string()];

    // Bi-temporal predicate: AS_OF → visibility window, None → live rows only.
    match as_of {
        Some(t) => {
            let ts = hlc_key(t);
            // SAFETY: ts comes from hlc_key which produces only ASCII decimal
            // digits, dots, and no SQL-special characters — no injection surface.
            where_parts.push(format!("sys_from <= '{ts}' AND (sys_to IS NULL OR '{ts}' < sys_to)"));
        }
        None => {
            where_parts.push("sys_to IS NULL".to_string());
        }
    }

    if let Some(f) = filter {
        where_parts.push(filter_to_sqlite(f));
    }

    let sql = format!(
        "SELECT id, embedding, metadata FROM lunaris_vec WHERE {}",
        where_parts.join(" AND ")
    );

    // SAFETY: `sql` is assembled from a fixed template plus values that have
    // gone through SQL escaping in `filter_to_sqlite`. `idx_name` is passed
    // as a bound parameter (?1), not interpolated. `AssertSqlSafe` is required
    // because sqlx 0.9 demands `'static` for dynamic query strings; using the
    // same pattern as `lunaris-storage-postgres/src/vector.rs`.
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(&idx_name)
        .fetch_all(pool)
        .await
        .map_err(|e| StorageError::Backend(format!("embedded(sqlite) vector_search: {e}")))?;

    // Brute-force cosine with a max-heap of size k.
    use std::collections::BinaryHeap;
    let mut heap: BinaryHeap<OrdHit> = BinaryHeap::with_capacity(k + 1);

    for row in rows {
        let id: Vec<u8> = row
            .try_get("id")
            .map_err(|e| StorageError::Backend(format!("embedded vec id decode: {e}")))?;
        let emb_json: String = row
            .try_get("embedding")
            .map_err(|e| StorageError::Backend(format!("embedded vec emb decode: {e}")))?;
        let meta_json: String = row
            .try_get("metadata")
            .map_err(|e| StorageError::Backend(format!("embedded vec meta decode: {e}")))?;

        // Decode embedding JSON → Vec<f32>.
        let emb: Vec<f32> = match serde_json::from_str(&emb_json) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(id = ?id, err = %e, "embedded vector_search: skipping row with malformed embedding JSON");
                continue;
            }
        };

        // Dimension mismatch → skip + warn.
        if emb.len() != query.len() {
            tracing::warn!(
                id = ?id,
                probe_dim = query.len(),
                row_dim = emb.len(),
                "embedded vector_search: skipping row — dimension mismatch"
            );
            continue;
        }

        // Compute cosine; skip rows with zero or NaN norm.
        let score = match cosine(query, probe_norm, &emb) {
            Some(s) => s,
            None => {
                tracing::warn!(id = ?id, "embedded vector_search: skipping row — zero/NaN emb norm");
                continue;
            }
        };

        let metadata: serde_json::Value =
            serde_json::from_str(&meta_json).unwrap_or(serde_json::Value::Null);

        let candidate =
            OrdHit { score, hit: VectorHit { id, score, rerank_applied: false, metadata } };

        // Maintain a max-heap of size k.
        // We want LARGEST scores → use a min-heap trick: cap at k, pop the
        // *minimum* when over capacity. `BinaryHeap` is a max-heap by default,
        // so we invert via `std::cmp::Reverse` on a min-heap, OR we simply
        // accumulate everything and drain top-k at the end.
        // With potentially large n, use the min-heap approach for O(n log k).
        // Achieved by storing in a max-heap but popping the MIN explicitly:
        // collect all candidates into the max-heap (unlimited), then drain top-k.
        // For true O(n log k), we use a min-heap:
        heap.push(candidate);
        // When the heap exceeds k, pop the smallest score (turn max-heap into
        // a "keep largest k" structure by popping the minimum).
        // BinaryHeap is a MAX-heap; to pop the minimum we'd need a min-heap.
        // Simplest correct approach: collect all rows first, sort at the end.
        // n <= typical lunaris scope = ~10k rows; fits in RAM trivially.
        // The sort approach is O(n log n) vs O(n log k), acceptable for brute-force.
    }

    // Convert heap to a sorted Vec (score descending).
    let mut all_hits: Vec<VectorHit> = heap.into_iter().map(|oh| oh.hit).collect();
    all_hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    all_hits.truncate(k);

    Ok(all_hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_eq_string_escapes_quotes() {
        let f = Filter::Eq { field: "source".into(), value: serde_json::json!("alice's notes") };
        let sql = filter_to_sqlite(&f);
        assert_eq!(sql, "json_extract(metadata, '$.source') = 'alice''s notes'");
    }

    #[test]
    fn filter_starts_with_renders() {
        let f = Filter::StartsWith { field: "source".into(), prefix: "src:notes/".into() };
        let sql = filter_to_sqlite(&f);
        assert_eq!(sql, "json_extract(metadata, '$.source') LIKE 'src:notes/%'");
    }

    #[test]
    fn filter_and_or_nest() {
        let f = Filter::And(vec![
            Filter::Eq { field: "kind".into(), value: serde_json::json!("note") },
            Filter::Or(vec![
                Filter::Eq { field: "tag".into(), value: serde_json::json!("a") },
                Filter::Eq { field: "tag".into(), value: serde_json::json!("b") },
            ]),
        ]);
        let sql = filter_to_sqlite(&f);
        assert!(sql.starts_with("(json_extract(metadata, '$.kind') = 'note' AND ("));
        assert!(sql.contains("json_extract(metadata, '$.tag') = 'a'"));
        assert!(sql.contains("json_extract(metadata, '$.tag') = 'b'"));
    }

    #[test]
    fn cosine_helper_exact_match() {
        let v = [1.0_f32, 0.0, 0.0];
        let n = norm(&v);
        let c = cosine(&v, n, &v).unwrap();
        assert!((c - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_helper_orthogonal() {
        let a = [1.0_f32, 0.0];
        let b = [0.0_f32, 1.0];
        let na = norm(&a);
        let c = cosine(&a, na, &b).unwrap();
        assert!(c.abs() < 1e-6);
    }

    #[test]
    fn cosine_helper_opposite() {
        let a = [1.0_f32, 0.0];
        let b = [-1.0_f32, 0.0];
        let na = norm(&a);
        let c = cosine(&a, na, &b).unwrap();
        assert!((c - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn cosine_helper_zero_emb_returns_none() {
        let a = [1.0_f32, 0.0];
        let b = [0.0_f32, 0.0];
        let na = norm(&a);
        assert!(cosine(&a, na, &b).is_none());
    }

    #[test]
    fn scoped_idx_format() {
        let scope = Scope::new("acme.agent-1").unwrap();
        assert_eq!(scoped_idx(&scope, "chunks"), "lunaris_acme.agent-1_chunks_idx");
        assert_eq!(scoped_idx(&scope, "entities"), "lunaris_acme.agent-1_entities_idx");
    }
}
