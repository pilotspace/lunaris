//! Per-scope cache for the SessionStart digest.
//!
//! # Why this exists
//!
//! Building a digest costs one `SCAN MATCH` keyspace walk per prefix. On Moon,
//! `MATCH` filters AFTER traversal, so a walk costs the same no matter how few
//! keys match — measured 2026-08-27 on a live 1.68M-key / 2.45GB store, a walk
//! matching **zero** keys still took 2.1s, and one matching 24,395 keys cost
//! the same as one matching 1.
//!
//! The cost tracks the store's total DATA volume rather than its key count: a
//! scratch Moon with the SAME key count (1.70M) but only 231MB walked in 0.18s,
//! ~10x faster. So this is not a clean O(DBSIZE) — but it is unconditional
//! overhead the digest pays and cannot amortize.
//!
//! The `SessionDigest` arm performed three such walks (episode prefix,
//! activation prefix, plus the spawned staleness sweep contending on the same
//! multiplexed connection) — measured ~7.6s floor, rising to ~19s once
//! `recent_by_source` hydrated all 24k episodes to return 5.
//!
//! The hook adapter budgets **400ms** for the digest and swallows every error
//! (`run_session_digest`'s bare `except Exception: return 0`), so a ~25x
//! overrun meant SessionStart injection silently never landed — exit 0, zero
//! bytes, every session.
//!
//! Reading [`digest_cache_key`] is a single O(1) HMGET instead.
//!
//! # Design for failure
//!
//! A digest is a nicety, never a gate — that contract is preserved here:
//!
//! * A cache **miss** returns `None`; the caller serves empty (exactly today's
//!   behavior) and rebuilds in the background. Never worse than before.
//! * A cache **read error** is indistinguishable from a miss — logged at debug,
//!   degraded to `None`. A corrupt/legacy payload decodes to `None` too.
//! * A cache **write error** is logged and dropped; the digest already served.
//! * A **stale** entry is still served (stale-while-revalidate). Serving a
//!   slightly old digest beats blocking a session start for seconds.
//!
//! [`digest_cache_key`]: lunaris_core::keyspace::digest_cache_key

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lunaris_core::{HlcClock, Scope, StoragePort, WriteOp, keyspace};
use serde::{Deserialize, Serialize};

use crate::context::ContextMemory;

/// How long a cached entry is considered fresh. A stale entry is still SERVED
/// (stale-while-revalidate); this only decides whether a background rebuild is
/// triggered. Override with `LUNARIS_CONTEXT_DIGEST_CACHE_TTL_MS`.
pub const DEFAULT_DIGEST_CACHE_TTL_MS: u64 = 15 * 60 * 1000;

/// The cached payload.
///
/// Stores the curated `memories` rather than a rendered string on purpose: the
/// caller still runs them through `finish_recall`, so per-request concerns
/// (`max_chars` budget, phase filtering, injection tracing, the activation
/// ledger write) keep behaving exactly as they did on the uncached path. Only
/// the expensive SCAN is elided.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DigestCacheEntry {
    /// Unix-epoch milliseconds at which this entry was built.
    pub built_at_ms: u64,
    /// The digest memories, already source-filtered, recency-sorted and capped.
    #[serde(default)]
    pub memories: Vec<ContextMemory>,
    /// Non-archived activation-ledger count, for the `/dream` nudge. Cached so
    /// the request path skips the second keyspace walk entirely.
    #[serde(default)]
    pub nudge_count: usize,
    /// `max_hits` this entry was built with. A request asking for MORE than was
    /// cached must not be silently served a short list, so the caller treats a
    /// larger ask as a miss.
    #[serde(default)]
    pub built_for_max_hits: usize,
}

/// Milliseconds since the Unix epoch. Saturates to 0 if the clock is before the
/// epoch — a nonsensical clock must not panic a session start.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Configured freshness window.
pub fn ttl_ms() -> u64 {
    std::env::var("LUNARIS_CONTEXT_DIGEST_CACHE_TTL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DIGEST_CACHE_TTL_MS)
}

impl DigestCacheEntry {
    /// Age in milliseconds, saturating at 0 for an entry stamped in the future
    /// (clock skew must not read as "infinitely fresh" OR panic).
    pub fn age_ms(&self, now: u64) -> u64 {
        now.saturating_sub(self.built_at_ms)
    }

    /// Whether a background rebuild should be triggered. Note this does NOT
    /// gate serving — a stale entry is served anyway.
    pub fn is_stale(&self, now: u64, ttl: u64) -> bool {
        self.age_ms(now) >= ttl
    }

    /// Whether this entry can satisfy a request asking for `max_hits`.
    ///
    /// An entry built for 5 hits cannot answer a request for 20 — it would
    /// silently under-serve. Built-for a LARGER count is fine: the caller
    /// truncates.
    pub fn satisfies(&self, max_hits: usize) -> bool {
        self.built_for_max_hits >= max_hits
    }
}

/// Read the cached digest for `scope`.
///
/// Returns `None` on miss, backend error, or undecodable payload — all three
/// are the same thing to the caller: "no usable cache, serve empty and
/// rebuild". Never returns `Err`; a cache is never allowed to fail a request.
pub async fn read(storage: &dyn StoragePort, scope: &Scope) -> Option<DigestCacheEntry> {
    let key = keyspace::digest_cache_key(scope);
    // "Latest" is a FRESH tick, not `Hlc::ZERO`. Moon's `read_as_of` ignores
    // the arg and returns current state, but the embedded/SQLite backend
    // really does filter `valid_from <= as_of` — so a zero HLC reads as
    // "before this row was written" and misses EVERY time. That divergence
    // would have looked correct on Moon and been silently dead everywhere
    // else. `HlcClock::new(0).tick()` is the same spelling
    // `read_episode_metadata` uses for an unpinned point-read.
    let read_at = HlcClock::new(0).tick();
    let row = match storage.read_as_of(scope, &key, read_at).await {
        Ok(Some(row)) => row,
        Ok(None) => return None,
        Err(err) => {
            tracing::debug!(err = %err, "digest cache: read failed, treating as miss");
            return None;
        }
    };
    match serde_json::from_slice::<DigestCacheEntry>(&row.value) {
        Ok(entry) => Some(entry),
        Err(err) => {
            tracing::debug!(err = %err, "digest cache: undecodable payload, treating as miss");
            None
        }
    }
}

/// Persist `entry` as the cached digest for `scope`.
///
/// Fail-open: a write error is logged at debug and dropped. The digest that
/// triggered this rebuild has already been served.
pub async fn write(storage: &Arc<dyn StoragePort>, scope: &Scope, entry: &DigestCacheEntry) {
    let value = match serde_json::to_vec(entry) {
        Ok(v) => v,
        Err(err) => {
            tracing::debug!(err = %err, "digest cache: encode failed, skipping write");
            return;
        }
    };
    let op = WriteOp::KvPut { key: keyspace::digest_cache_key(scope), value };
    if let Err(err) = storage.atomic_write(scope, &[op]).await {
        tracing::debug!(err = %err, "digest cache: write failed, cache stays cold");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(built_at_ms: u64, max_hits: usize) -> DigestCacheEntry {
        DigestCacheEntry {
            built_at_ms,
            memories: vec![],
            nudge_count: 0,
            built_for_max_hits: max_hits,
        }
    }

    #[test]
    fn age_saturates_for_a_future_stamp() {
        // Clock skew must not panic (underflow) nor read as infinitely fresh.
        assert_eq!(entry(9_000, 5).age_ms(1_000), 0);
    }

    #[test]
    fn staleness_is_measured_against_the_ttl() {
        let e = entry(1_000, 5);
        assert!(!e.is_stale(1_500, 1_000), "500ms old under a 1s ttl is fresh");
        assert!(e.is_stale(2_000, 1_000), "1000ms old under a 1s ttl is stale");
        assert!(e.is_stale(2_001, 1_000));
    }

    #[test]
    fn an_entry_cannot_satisfy_a_larger_ask() {
        let e = entry(0, 5);
        assert!(e.satisfies(5), "exact ask is satisfiable");
        assert!(e.satisfies(3), "smaller ask is satisfiable — caller truncates");
        assert!(!e.satisfies(6), "a larger ask must MISS, not silently under-serve");
    }

    #[test]
    fn payload_round_trips_and_tolerates_missing_optional_fields() {
        let e = entry(42, 7);
        let back: DigestCacheEntry =
            serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
        assert_eq!(back.built_at_ms, 42);
        assert_eq!(back.built_for_max_hits, 7);

        // A payload written by an older shape must decode, not poison the cache.
        let minimal: DigestCacheEntry = serde_json::from_str(r#"{"built_at_ms":1}"#).unwrap();
        assert_eq!(minimal.built_at_ms, 1);
        assert!(minimal.memories.is_empty());
        assert_eq!(minimal.nudge_count, 0);
    }

    /// Read-after-write against a REAL backend.
    ///
    /// Regression pin: `read` originally passed `Hlc::default()` (== ZERO) as
    /// the as-of. Moon ignores that arg, so the cache appeared to work there —
    /// but the embedded backend filters `valid_from <= as_of`, so a zero HLC
    /// read as "before the row existed" and MISSED every single time. A
    /// serde-only round-trip test cannot see this; it needs a real store.
    #[tokio::test]
    async fn entry_round_trips_through_a_real_backend() {
        let scope = Scope::new("test-digest-cache-roundtrip").unwrap();
        // `harness` owns the ephemeral Moon child process; it must outlive the
        // reads below. 0.7.0 removed the `memory://` fallback these tests used.
        let harness = lunaris_test_harness::open_test_storage().await;
        let storage = harness.port();

        assert!(read(storage.as_ref(), &scope).await.is_none(), "cold cache must miss");

        let e = DigestCacheEntry {
            built_at_ms: now_ms(),
            memories: vec![],
            nudge_count: 17,
            built_for_max_hits: 8,
        };
        write(&storage, &scope, &e).await;

        let back = read(storage.as_ref(), &scope)
            .await
            .expect("a written entry must be readable back — as-of must mean NOW");
        assert_eq!(back.nudge_count, 17);
        assert_eq!(back.built_for_max_hits, 8);
    }

    #[test]
    fn ttl_default_applies_when_env_is_unset_or_garbage() {
        // Not using env::set_var — the crate is #![forbid(unsafe_code)] and the
        // parse rule is what matters here.
        let parse = |v: Option<&str>| {
            v.and_then(|v| v.parse::<u64>().ok()).unwrap_or(DEFAULT_DIGEST_CACHE_TTL_MS)
        };
        assert_eq!(parse(None), DEFAULT_DIGEST_CACHE_TTL_MS);
        assert_eq!(parse(Some("not-a-number")), DEFAULT_DIGEST_CACHE_TTL_MS);
        assert_eq!(parse(Some("1234")), 1234);
    }
}
