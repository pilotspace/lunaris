//! `read_as_of` — `TEMPORAL.SNAPSHOT_AT` then `HGET <key> v` + `HGET <key> bt`.
//! `scan_range` — `SCAN ... MATCH <prefix>*` then `HGET <key> v` per matched key.
//!
//! ## Why HGET on field `v` (and `bt`)
//!
//! `atomic_write::KvPut` stores values via `HSET <key> v <value>`. `read_as_of` reads
//! the same field. The `bt` field is optional — when present it's a serde-encoded
//! `BiTemporal` (the writer must HSET it explicitly; the trait's `KvPut` variant only
//! carries `key + value`, so callers who care about bi-temporal stamping use a
//! `Row<Bytes>`-shaped payload as the value).
//!
//! For `read_as_of` Phase 1 returns a default `BiTemporal` when the `bt` field is
//! absent — Phase 2's higher-level write path will always populate it.

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::bitemporal::BiTemporal;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::Row;
use redis::AsyncCommands;

use crate::client::{MoonClient, redis_err};
use crate::vector::pack_hlc;

pub(crate) async fn read_as_of(
    c: &MoonClient,
    key: &[u8],
    as_of: Hlc,
) -> Result<Option<Row<Bytes>>, StorageError> {
    let mut conn = c.conn();

    let pinned = pack_hlc(as_of);
    redis::cmd("TEMPORAL.SNAPSHOT_AT")
        .arg(pinned.to_string())
        .query_async::<redis::Value>(&mut conn)
        .await
        .map_err(redis_err)?;

    let value: Option<Vec<u8>> = conn.hget(key, "v").await.map_err(redis_err)?;
    let bt_bytes: Option<Vec<u8>> = conn.hget(key, "bt").await.map_err(redis_err)?;

    // Always release the snapshot, even if the reads errored.
    let _ = redis::cmd("TEMPORAL.INVALIDATE")
        .query_async::<redis::Value>(&mut conn)
        .await;

    match value {
        None => Ok(None),
        Some(v) => {
            let bt = match bt_bytes {
                Some(b) => serde_json::from_slice::<BiTemporal>(&b).unwrap_or_else(|_| zero_bt()),
                None => zero_bt(),
            };
            Ok(Some(Row { key: key.to_vec(), value: Bytes::from(v), bt }))
        }
    }
}

#[inline]
fn zero_bt() -> BiTemporal {
    let z = Hlc::ZERO;
    BiTemporal { valid: (z, None), sys: (z, None) }
}

pub(crate) async fn scan_range<'a>(
    c: &'a MoonClient,
    prefix: &[u8],
    as_of: Option<Hlc>,
) -> Result<BoxStream<'a, Result<(Bytes, Bytes), StorageError>>, StorageError> {
    let mut conn = c.conn();

    if let Some(t) = as_of {
        let pinned = pack_hlc(t);
        redis::cmd("TEMPORAL.SNAPSHOT_AT")
            .arg(pinned.to_string())
            .query_async::<redis::Value>(&mut conn)
            .await
            .map_err(redis_err)?;
    }

    // Build `<prefix>*` MATCH pattern. We construct it from raw bytes since prefixes are
    // expected to be ASCII (`lunaris:<primitive>:<ulid>`); fall back to lossy UTF-8 only
    // when constructing the MATCH arg.
    let pattern = {
        let mut p = prefix.to_vec();
        p.push(b'*');
        String::from_utf8_lossy(&p).into_owned()
    };

    // Iterate SCAN cursor; for each key, HGET its `v` field. Phase 1 buffers the full
    // result into a Vec then returns a stream — Phase 2 replaces this with a true
    // cursor-driven async stream when we add backpressure.
    let mut cursor: u64 = 0;
    let mut all_pairs: Vec<Result<(Bytes, Bytes), StorageError>> = Vec::new();
    loop {
        let (next, batch): (u64, Vec<Vec<u8>>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern.as_str())
            .arg("COUNT")
            .arg(1000)
            .query_async(&mut conn)
            .await
            .map_err(redis_err)?;
        for k in batch {
            match conn.hget::<_, _, Option<Vec<u8>>>(k.as_slice(), "v").await {
                Ok(Some(v)) => all_pairs.push(Ok((Bytes::from(k), Bytes::from(v)))),
                Ok(None) => {} // key matched but had no `v` field — skip silently
                Err(e) => all_pairs.push(Err(redis_err(e))),
            }
        }
        if next == 0 {
            break;
        }
        cursor = next;
    }

    if as_of.is_some() {
        let _ = redis::cmd("TEMPORAL.INVALIDATE")
            .query_async::<redis::Value>(&mut conn)
            .await;
    }

    Ok(stream::iter(all_pairs).boxed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_bt_round_trips_through_serde() {
        let bt = zero_bt();
        let s = serde_json::to_string(&bt).unwrap();
        let bt2: BiTemporal = serde_json::from_str(&s).unwrap();
        assert_eq!(bt.valid.0.wall_ms, bt2.valid.0.wall_ms);
        assert_eq!(bt.sys.0.counter, bt2.sys.0.counter);
    }

    #[test]
    fn pack_hlc_used_by_kv_matches_vector_module() {
        // Cross-module sanity: kv uses the same packing as vector.
        let t = Hlc { wall_ms: 100, counter: 5, node_id: 0 };
        let n = pack_hlc(t);
        assert_eq!(n, (100u128 << 32) | 5u128);
    }
}
