//! `read_as_of` — bi-temporal predicate on `lunaris_kv`.
//! `scan_range`  — byte-range prefix scan on `lunaris_kv.key` + bi-temporal predicate.
//!
//! The bi-temporal predicate emulates Moon's `TEMPORAL.SNAPSHOT_AT` purely in SQL:
//!
//! ```sql
//!   valid_from <= $ts AND (valid_to IS NULL OR $ts < valid_to)
//!   AND sys_from <= $ts AND (sys_to IS NULL OR $ts < sys_to)
//! ```
//!
//! `scan_range` buffers the matched rows into a `Vec` then returns
//! `stream::iter(...)` for v0 simplicity (matches Plan 03's MoonStorage pattern).
//! Phase 2 swaps to a true cursor-driven async stream when backpressure is added.

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::bitemporal::BiTemporal;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::scope::Scope;
use lunaris_core::storage::types::Row as LRow;
use sqlx::Row as SqlxRow;

use crate::pool::{PgClient, sqlx_err};
use crate::schema::hlc_to_ts;

pub(crate) async fn read_as_of(
    c: &PgClient,
    scope: &Scope,
    key: &[u8],
    as_of: Hlc,
) -> Result<Option<LRow<Bytes>>, StorageError> {
    let ts = hlc_to_ts(as_of);
    // RFC 0001 Wave 1B: SET LOCAL so the scope GUC is active for this read.
    // lunaris_kv does not have RLS, but the GUC is set for consistency so
    // any future RLS on lunaris_kv automatically picks it up.
    let mut tx = c.pool.begin().await.map_err(sqlx_err)?;
    sqlx::query("SET LOCAL lunaris.scope = $1")
        .bind(scope.as_str())
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;

    let row_opt = sqlx::query(
        "SELECT key, value, bt FROM lunaris_kv \
         WHERE key = $1 \
           AND valid_from <= $2 AND (valid_to IS NULL OR $2 < valid_to) \
           AND sys_from   <= $2 AND (sys_to   IS NULL OR $2 < sys_to) \
         ORDER BY valid_from_hlc DESC \
         LIMIT 1",
    )
    .bind(key)
    .bind(ts)
    .fetch_optional(&mut *tx)
    .await
    .map_err(sqlx_err)?;

    tx.commit().await.map_err(sqlx_err)?;

    Ok(row_opt.map(|r| {
        let k: Vec<u8> = r.try_get("key").unwrap_or_default();
        let v: Vec<u8> = r.try_get("value").unwrap_or_default();
        let bt_json: serde_json::Value = r.try_get("bt").unwrap_or(serde_json::Value::Null);
        let bt: BiTemporal = serde_json::from_value(bt_json).unwrap_or_else(|_| {
            let z = lunaris_core::hlc::Hlc::ZERO;
            BiTemporal { valid: (z, None), sys: (z, None) }
        });
        LRow { key: k, value: Bytes::from(v), bt }
    }))
}

/// Exclusive upper bound for a byte-wise prefix scan.
///
/// Returns `Some(successor)` where `successor` is the smallest byte string
/// strictly greater than every string starting with `prefix`. Returns `None`
/// if `prefix` is empty or consists entirely of `0xFF` bytes — in that case
/// the scan is open-ended above and the caller should omit the upper bound.
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    for i in (0..out.len()).rev() {
        if out[i] < 0xFF {
            out[i] += 1;
            out.truncate(i + 1);
            return Some(out);
        }
    }
    None
}

pub(crate) async fn scan_range<'a>(
    c: &'a PgClient,
    scope: &Scope,
    prefix: &[u8],
    as_of: Option<Hlc>,
) -> Result<BoxStream<'a, Result<(Bytes, Bytes), StorageError>>, StorageError> {
    // `key` is BYTEA — `LIKE` is undefined on bytea (`bytea ~~ text` has no
    // operator). Use a byte-wise range scan on the primary-key B-tree
    // instead: `key >= prefix AND key < successor(prefix)`.
    let lower = prefix.to_vec();
    let upper = prefix_upper_bound(prefix);

    // RFC 0001 Wave 1B: SET LOCAL inside a transaction so the scope GUC is
    // scoped to this connection use only.
    let mut tx = c.pool.begin().await.map_err(sqlx_err)?;
    sqlx::query("SET LOCAL lunaris.scope = $1")
        .bind(scope.as_str())
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;

    let rows = match (as_of, upper) {
        (Some(t), Some(u)) => {
            let ts = hlc_to_ts(t);
            sqlx::query(
                "SELECT key, value FROM lunaris_kv \
                 WHERE key >= $1 AND key < $2 \
                   AND valid_from <= $3 AND (valid_to IS NULL OR $3 < valid_to) \
                   AND sys_from   <= $3 AND (sys_to   IS NULL OR $3 < sys_to)",
            )
            .bind(lower)
            .bind(u)
            .bind(ts)
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_err)?
        }
        (Some(t), None) => {
            let ts = hlc_to_ts(t);
            sqlx::query(
                "SELECT key, value FROM lunaris_kv \
                 WHERE key >= $1 \
                   AND valid_from <= $2 AND (valid_to IS NULL OR $2 < valid_to) \
                   AND sys_from   <= $2 AND (sys_to   IS NULL OR $2 < sys_to)",
            )
            .bind(lower)
            .bind(ts)
            .fetch_all(&mut *tx)
            .await
            .map_err(sqlx_err)?
        }
        (None, Some(u)) => sqlx::query(
            "SELECT key, value FROM lunaris_kv \
             WHERE key >= $1 AND key < $2 AND valid_to IS NULL",
        )
        .bind(lower)
        .bind(u)
        .fetch_all(&mut *tx)
        .await
        .map_err(sqlx_err)?,
        (None, None) => sqlx::query(
            "SELECT key, value FROM lunaris_kv \
             WHERE key >= $1 AND valid_to IS NULL",
        )
        .bind(lower)
        .fetch_all(&mut *tx)
        .await
        .map_err(sqlx_err)?,
    };

    tx.commit().await.map_err(sqlx_err)?;

    let pairs: Vec<Result<(Bytes, Bytes), StorageError>> = rows
        .into_iter()
        .map(|r| {
            let k: Vec<u8> = r.try_get("key").unwrap_or_default();
            let v: Vec<u8> = r.try_get("value").unwrap_or_default();
            Ok((Bytes::from(k), Bytes::from(v)))
        })
        .collect();

    Ok(stream::iter(pairs).boxed())
}
