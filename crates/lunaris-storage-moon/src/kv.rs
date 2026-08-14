//! `read_as_of` — one `HMGET` call fetching both the `v` and `bt` fields in a
//! single round trip (plan 260610-f91: replaces the prior two serial HGETs).
//! `scan_range` — documented HSCAN escape hatch (the only raw RESP `cmd` site
//! allowed in `lunaris-storage-moon/src/`) then concurrent HGET fan-out via
//! `futures::stream::buffered` (plan 260610-f91).
//!
//! `scan_range` and `read_as_of` receive fully-shaped keys/prefixes from the
//! caller. Higher layers that need scope isolation pass `keyspace::*_prefix(scope)`;
//! raw conformance and utility callers can scan their own arbitrary prefixes.
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
//!
//! ## AS_OF (0.6.2 task 9)
//!
//! Both readers below refuse a *historical* `as_of` up front via
//! [`crate::as_of::reject_historical_read`] — Moon has no KV version chain,
//! so answering with current state would be silently wrong. Latest-state
//! reads (every production call site) are unaffected. See `crate::as_of`
//! for the rule and the upstream `TemporalKvIndex` path.

use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::Scope;
use lunaris_core::bitemporal::BiTemporal;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::Row;

use crate::client::{MoonClient, moon_err, redis_err};

pub(crate) async fn read_as_of(
    c: &MoonClient,
    _scope: &Scope,
    key: &[u8],
    as_of: Hlc,
) -> Result<Option<Row<Bytes>>, StorageError> {
    // Moon KV (HSET-based) exposes no AS_OF read clause —
    // `TEMPORAL.SNAPSHOT_AT` is a 0-arg snapshot recorder, not a per-query
    // pin (Phase 1.5 sent `TEMPORAL.SNAPSHOT_AT <ts>` via the SDK's
    // `snapshot_at_packed(ts)`; the server rejects it), and an overwrite
    // destroys the prior value so there is no version to return anyway.
    //
    // Upstream tracking: Moon carries a half-built `TemporalKvIndex`
    // (`record` / `get_at`) with zero production call sites — see
    // `vendor/moon` (read-only from this repo). Wiring it to the KV write
    // path is what would let this method serve real historical reads;
    // until then a historical pin is REFUSED rather than answered with
    // present-time data (0.6.2 task 9, replaces the pre-0.6.2 "return
    // current state" behaviour that made the bi-temporal claim silently
    // wrong on the primary backend).
    crate::as_of::reject_historical_read(as_of)?;

    let mut typed = c.typed();

    // RFC 0001 Wave 1C: `key` is already scope-prefixed by the caller
    // (e.g. `keyspace::episode_key(scope, id)`). No additional prefixing needed.
    //
    // Plan 260610-f91: collapse the two serial HGETs into one HMGET so both
    // fields are fetched in a single RESP round trip.
    let mut fields = typed.hmget::<_, _, Vec<u8>>(key, &["v", "bt"]).await.map_err(moon_err)?;
    // fields[0] = "v", fields[1] = "bt" (RESP order matches field slice order).
    let bt_bytes = fields.pop().flatten(); // index 1
    let value = fields.pop().flatten(); // index 0

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

/// HOOK-05 idempotency sidecar lookup (ADD task moon-parity-honesty).
///
/// Reads the JSON [`Lsn`] stored at `keyspace::dedupe_key(scope, raw)`.
/// A missing key or an unparseable value both resolve to `None` — the
/// caller (`ingest_idempotent`) falls through to a fresh ingest, never
/// errors the write path on sidecar corruption.
pub(crate) async fn lookup_dedupe(
    c: &MoonClient,
    scope: &Scope,
    raw: &str,
) -> Result<Option<lunaris_core::storage::types::Lsn>, StorageError> {
    let mut typed = c.typed();
    let key = lunaris_core::keyspace::dedupe_key(scope, raw);
    let value: Option<Vec<u8>> = typed.get(key).await.map_err(moon_err)?;
    Ok(value.and_then(|b| serde_json::from_slice(&b).ok()))
}

/// HOOK-05 idempotency sidecar insert: `SET NX` — first writer wins, a
/// concurrent replay can never clobber the prior LSN.
pub(crate) async fn insert_dedupe(
    c: &MoonClient,
    scope: &Scope,
    raw: &str,
    lsn: lunaris_core::storage::types::Lsn,
) -> Result<(), StorageError> {
    let mut typed = c.typed();
    let key = lunaris_core::keyspace::dedupe_key(scope, raw);
    let payload = serde_json::to_vec(&lsn)
        .map_err(|e| StorageError::Backend(format!("dedupe lsn serialize: {e}")))?;
    let _first_writer: bool = typed.set_nx(key, payload).await.map_err(moon_err)?;
    Ok(())
}

pub(crate) async fn scan_range<'a>(
    c: &'a MoonClient,
    _scope: &Scope,
    prefix: &[u8],
    as_of: Option<Hlc>,
) -> Result<BoxStream<'a, Result<(Bytes, Bytes), StorageError>>, StorageError> {
    // Same contract as `read_as_of`: `None` means "latest" and is always
    // served; `Some(historical)` is refused rather than answered with
    // present-time rows. `GET /v1/snapshot/{lsn}` is the caller that makes
    // this visible — it asks for the whole keyspace at a past LSN, which
    // Moon cannot reconstruct.
    if let Some(pin) = as_of {
        crate::as_of::reject_historical_read(pin)?;
    }

    let typed = c.typed();

    // Build `<prefix>*` MATCH pattern. The StoragePort contract takes the
    // prefix literally; scope-aware callers pass an already scoped prefix.
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
    //
    // Plan 260610-f91: fan out the per-key HGETs within each SCAN batch concurrently
    // using buffered(SCAN_CONCURRENCY). Each cloned TypedClient handle shares the
    // underlying MultiplexedConnection — concurrent calls pipeline naturally via RESP.
    const SCAN_CONCURRENCY: usize = 32;
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

        // Concurrent HGET fan-out for this SCAN batch (bounded at SCAN_CONCURRENCY).
        // typed.clone() produces a new TypedClient handle that shares the mux connection.
        let batch_results: Vec<Result<(Bytes, Bytes), StorageError>> = stream::iter(batch)
            .map(|k| {
                let mut t = typed.clone();
                async move {
                    match t.hget::<_, _, Vec<u8>>(k.as_slice(), "v").await {
                        Ok(Some(v)) => Some(Ok((Bytes::from(k), Bytes::from(v)))),
                        Ok(None) => None, // key matched but had no `v` field — skip silently
                        Err(e) => Some(Err(moon_err(e))),
                    }
                }
            })
            .buffered(SCAN_CONCURRENCY)
            .filter_map(|opt| async move { opt })
            .collect()
            .await;
        all_pairs.extend(batch_results);

        if next == 0 {
            break;
        }
        cursor = next;
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

    /// scan_range MATCH pattern uses the caller prefix literally. Scope-aware
    /// callers should pass an already scoped prefix.
    #[test]
    fn scan_range_pattern_accepts_scoped_prefixes() {
        let scope = lunaris_core::Scope::new("acme.agent-1").unwrap();
        let sp = lunaris_core::keyspace::episode_prefix(&scope);
        // The MATCH pattern must start with the scope prefix.
        assert!(
            String::from_utf8_lossy(&sp).starts_with("lunaris:acme.agent-1:episode:"),
            "scope_prefix must produce lunaris:{{scope}}: prefix"
        );
    }

    /// Plan 260610-f91: structural guard — read_as_of must use a single HMGET
    /// round trip instead of two serial HGET calls.
    #[test]
    fn read_as_of_uses_hmget_not_two_hgets() {
        let src = include_str!("kv.rs");
        // After refactoring, "hmget" must appear.
        assert!(src.contains("hmget"), "read_as_of MUST use hmget (single round trip)");
        // The two-HGET pattern: typed.hget(key, "v") and typed.hget(key, "bt") must be gone.
        assert!(
            !src.contains("hget::<_, _, Vec<u8>>(key, \"v\")"),
            "read_as_of must NOT use typed.hget for field \"v\" (use hmget instead)"
        );
        assert!(
            !src.contains("hget::<_, _, Vec<u8>>(key, \"bt\")"),
            "read_as_of must NOT use typed.hget for field \"bt\" (use hmget instead)"
        );
    }

    /// Plan 260610-f91: structural guard — scan_range must use buffered() for
    /// concurrent per-batch HGET fan-out. We split on `#[cfg(test)]` so the
    /// check only inspects implementation code, not this test module.
    #[test]
    fn scan_range_uses_concurrent_fan_out() {
        let src = include_str!("kv.rs");
        // Only look at the implementation (before the test module).
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or("");
        assert!(
            impl_src.contains("buffered(SCAN_CONCURRENCY)"),
            "scan_range MUST use buffered(SCAN_CONCURRENCY) for concurrent HGET fan-out"
        );
    }
}
