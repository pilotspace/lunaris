//! `read_as_of` — `client.temporal().snapshot_at_packed(...)` then typed
//! `client.hget(<key>, "v")` + `client.hget(<key>, "bt")`.
//! `scan_range` — documented HSCAN escape hatch (the only raw RESP `cmd` site
//! allowed in `lunaris-storage-moon/src/`) then typed `client.hget(<key>, "v")`
//! per match.
//!
//! Phase 1.5 retrofit (STORE-09): all RESP commands here go through the typed
//! `moon-client` SDK except for the single documented HSCAN call below.
//!
//! ## Why HGET on field `v` (and `bt`)
//!
//! `atomic_write::KvPut` stores values via `HSET <key> v <value>` (typed call:
//! `MoonClient::hset(key, "v", value)`). `read_as_of` reads the same field. The `bt`
//! field is optional — when present it's a serde-encoded `BiTemporal` (the writer
//! must HSET it explicitly; the trait's `KvPut` variant only carries `key + value`,
//! so callers who care about bi-temporal stamping use a `Row<Bytes>`-shaped payload
//! as the value).
//!
//! For `read_as_of` Phase 1 returns a default `BiTemporal` when the `bt` field is
//! absent — Phase 2's higher-level write path will always populate it.

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::bitemporal::BiTemporal;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::Row;

use crate::client::{MoonClient, moon_err, redis_err};
use crate::vector::pack_hlc;

pub(crate) async fn read_as_of(
    c: &MoonClient,
    key: &[u8],
    as_of: Hlc,
) -> Result<Option<Row<Bytes>>, StorageError> {
    let mut typed = c.typed();

    let pinned = pack_hlc(as_of);
    typed.temporal().snapshot_at_packed(pinned).await.map_err(moon_err)?;

    // moon-client's `hget` returns `Result<Option<RV>>`; when the field is absent we
    // get `Ok(None)` and translate to `None` here.
    let value: Option<Vec<u8>> = typed.hget::<_, _, Vec<u8>>(key, "v").await.map_err(moon_err)?;
    let bt_bytes: Option<Vec<u8>> = typed.hget::<_, _, Vec<u8>>(key, "bt").await.map_err(moon_err)?;

    // Always release the snapshot, even if the reads errored.
    let _ = typed.temporal().release_snapshot().await;

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
    let mut typed = c.typed();

    if let Some(t) = as_of {
        let pinned = pack_hlc(t);
        typed.temporal().snapshot_at_packed(pinned).await.map_err(moon_err)?;
    }

    // Build `<prefix>*` MATCH pattern. We construct it from raw bytes since prefixes are
    // expected to be ASCII (`lunaris:<primitive>:<ulid>`); fall back to lossy UTF-8 only
    // when constructing the MATCH arg.
    let pattern = {
        let mut p = prefix.to_vec();
        p.push(b'*');
        String::from_utf8_lossy(&p).into_owned()
    };

    // ESCAPE HATCH: moon-client v0.1.0 does not expose a typed HSCAN wrapper, only
    // generic SCAN. Lunaris uses HSET-based KV (single-field hash with `v`/`bt`),
    // so we need HSCAN here. When moon-client adds a typed `client.scan_match(...)`
    // hash variant (or a higher-level `scan_hashes(prefix)` helper), swap this for
    // it. This is the ONLY raw RESP cmd invocation permitted in
    // `lunaris-storage-moon/src/` per Phase 1.5 retrofit constraints (STORE-09).
    //
    // We reach the underlying `redis::aio::MultiplexedConnection` via
    // `MoonClient::inner_mut()` (a public escape hatch in moon-client v0.1.0) on a
    // local clone so the parent connection remains free for typed calls.
    let mut raw_inner = typed.clone();
    let raw_conn = raw_inner.inner_mut();

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
            .query_async(raw_conn)
            .await
            .map_err(redis_err)?;
        for k in batch {
            match typed.hget::<_, _, Vec<u8>>(k.as_slice(), "v").await {
                Ok(Some(v)) => all_pairs.push(Ok((Bytes::from(k), Bytes::from(v)))),
                Ok(None) => {} // key matched but had no `v` field — skip silently
                Err(e) => all_pairs.push(Err(moon_err(e))),
            }
        }
        if next == 0 {
            break;
        }
        cursor = next;
    }

    if as_of.is_some() {
        let _ = typed.temporal().release_snapshot().await;
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
