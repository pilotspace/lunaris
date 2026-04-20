//! `read_as_of` — bi-temporal predicate on `lunaris_kv`.
//! `scan_range`  — prefix `LIKE` on `lunaris_kv.key` + bi-temporal predicate.
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
use lunaris_core::storage::types::Row as LRow;
use sqlx::Row as SqlxRow;

use crate::pool::{PgClient, sqlx_err};
use crate::schema::hlc_to_ts;

pub(crate) async fn read_as_of(
    c: &PgClient,
    key: &[u8],
    as_of: Hlc,
) -> Result<Option<LRow<Bytes>>, StorageError> {
    let ts = hlc_to_ts(as_of);
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
    .fetch_optional(&c.pool)
    .await
    .map_err(sqlx_err)?;

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

pub(crate) async fn scan_range<'a>(
    c: &'a PgClient,
    prefix: &[u8],
    as_of: Option<Hlc>,
) -> Result<BoxStream<'a, Result<(Bytes, Bytes), StorageError>>, StorageError> {
    // Escape LIKE-significant wildcards in the user-supplied prefix.
    let prefix_pat = {
        let lossy = String::from_utf8_lossy(prefix);
        let escaped = lossy.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        format!("{escaped}%")
    };

    let rows = match as_of {
        Some(t) => {
            let ts = hlc_to_ts(t);
            sqlx::query(
                "SELECT key, value FROM lunaris_kv \
                 WHERE key LIKE $1 \
                   AND valid_from <= $2 AND (valid_to IS NULL OR $2 < valid_to) \
                   AND sys_from   <= $2 AND (sys_to   IS NULL OR $2 < sys_to)",
            )
            .bind(prefix_pat)
            .bind(ts)
            .fetch_all(&c.pool)
            .await
            .map_err(sqlx_err)?
        }
        None => sqlx::query(
            "SELECT key, value FROM lunaris_kv \
                 WHERE key LIKE $1 AND valid_to IS NULL",
        )
        .bind(prefix_pat)
        .fetch_all(&c.pool)
        .await
        .map_err(sqlx_err)?,
    };

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
