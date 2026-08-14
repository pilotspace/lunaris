//! AS_OF honesty guard for the Moon KV surface (0.6.2 task 9).
//!
//! # What Moon can and cannot do
//!
//! Lunaris KV rows live in Moon as plain hashes (`HSET <key> v <value>`,
//! see [`crate::kv`]). `HGET` / `HMGET` accept no temporal clause and Moon
//! keeps **no KV version chain**: an overwrite destroys the prior value.
//! `TEMPORAL.SNAPSHOT_AT` is a 0-arg snapshot *recorder*, not a per-query
//! pin — Phase 1.5 tried `TEMPORAL.SNAPSHOT_AT <ts>` and the server
//! rejected it.
//!
//! Upstream has a half-built `TemporalKvIndex` (`record` / `get_at`, see
//! `vendor/moon` — read-only here) with **zero production call sites**;
//! wiring it is the tracked upstream path to real KV time travel. Until it
//! ships and Lunaris routes reads through it, this module is the contract.
//!
//! # The rule
//!
//! `read_as_of` / `scan_range` carry two request shapes down one method:
//!
//! * **Latest-state read** — `as_of` is "now". Every production call site
//!   is this shape: `hydrate`, `forget`, the verify worker, `/v1/episode`
//!   and `/v1/detail` all tick the clock immediately before the read.
//!   Current state IS the correct answer, so Moon serves it.
//! * **Historical read** — `as_of` is meaningfully in the past. Moon
//!   cannot answer it. Returning current state (or a bare `Ok(None)`,
//!   which reads as "nothing existed then") is a silent correctness
//!   violation, so the request is refused with
//!   [`StorageError::NotSupported`] instead. `lunaris-server`'s `map_error`
//!   already renders that variant as `501 { "error": "not_supported" }`.
//!
//! # Why a wall-clock window and not a flag
//!
//! The two shapes are indistinguishable at the port boundary: both arrive
//! as one `Hlc`, and threading an "is this a real pin?" flag would mean
//! changing `StoragePort` and all ~60 call sites. The discriminator is
//! therefore the timestamp itself, with a deliberately generous
//! [`AS_OF_LIVE_WINDOW_MS`] tolerance:
//!
//! * A false *positive* (a live read misread as historical) would 501 a
//!   working query — a production outage. The window must cover the
//!   longest legitimate gap between pinning `as_of` and issuing the read.
//!   The worst case in-tree is `RetrievalBuilder::execute`, which pins one
//!   snapshot up front and hydrates after the whole fan-out (vector +
//!   graph + BM25 + a possibly cold-loading reranker): seconds, not hours.
//! * A false *negative* (a historical read inside the window served as
//!   latest) merely preserves today's behaviour for a pin a few minutes
//!   old, which is the least interesting case. Real time-travel queries
//!   ("what did I know last week") are hours to days back and always trip
//!   the guard.
//!
//! One hour is far past any in-tree pin-to-read gap and far short of any
//! meaningful time-travel query.

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;

/// How far in the past an `as_of` may sit and still count as a
/// latest-state read. See the module doc for the asymmetry that sets this.
pub const AS_OF_LIVE_WINDOW_MS: u64 = 60 * 60 * 1_000;

/// Moon's answer to [`lunaris_core::StoragePort::supports_historical_kv_reads`].
///
/// `false` until Moon's `TemporalKvIndex` (`record` / `get_at`) is wired to
/// the KV write path and Lunaris reads through it.
pub const HISTORICAL_KV_READS: bool = false;

/// The one error a historical Moon KV read produces. `&'static str` payload
/// per the `StorageError::NotSupported` idiom; the `moon_kv_as_of` prefix is
/// the greppable machine token, the rest is what an operator reading a 501
/// body needs.
const AS_OF_UNSUPPORTED: &str = "moon_kv_as_of: Moon KV read_as_of/scan_range cannot answer a \
                                 historical snapshot — Moon stores Lunaris rows as plain hashes \
                                 and HGET/HMGET accept no AS_OF clause, so no prior version \
                                 exists to return. Latest-state reads are supported. For as-of \
                                 reads use a backend that reports \
                                 supports_historical_kv_reads() == true";

/// Wall-clock millis since the Unix epoch.
#[inline]
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Is `as_of` a *historical* pin relative to `now_ms`?
///
/// Pure and total so the policy is unit-testable without a clock or a Moon.
/// A future `as_of` (the conformance suite pins `u64::MAX / 2` post-commit)
/// is never historical.
#[inline]
#[must_use]
pub fn is_historical(as_of: Hlc, now_ms: u64) -> bool {
    as_of.wall_ms.saturating_add(AS_OF_LIVE_WINDOW_MS) < now_ms
}

/// Refuse a historical KV read before it reaches Moon.
///
/// Returns `Ok(())` for latest-state reads (the whole production hot path)
/// and `Err(StorageError::NotSupported(_))` for a genuine historical pin.
/// Callers MUST invoke this before issuing any RESP command: a rejected
/// read costs zero round trips, and an error raised *after* a successful
/// fetch would be indistinguishable from a transport failure.
pub fn reject_historical_read(as_of: Hlc) -> Result<(), StorageError> {
    let now_ms = now_millis();
    if is_historical(as_of, now_ms) {
        tracing::warn!(
            requested_wall_ms = as_of.wall_ms,
            now_wall_ms = now_ms,
            window_ms = AS_OF_LIVE_WINDOW_MS,
            "moon: refusing historical KV read — Moon has no KV version chain"
        );
        return Err(StorageError::NotSupported(AS_OF_UNSUPPORTED));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    #[test]
    fn epoch_pin_is_historical() {
        assert!(is_historical(Hlc::ZERO, NOW));
        assert!(is_historical(Hlc::from_parts(1_000, 0, 0), NOW));
    }

    #[test]
    fn now_and_future_pins_are_latest() {
        assert!(!is_historical(Hlc::from_parts(NOW, 0, 0), NOW));
        assert!(!is_historical(Hlc::from_parts(NOW + 5_000, 0, 0), NOW));
        // The conformance suite's post-commit "generous now".
        assert!(!is_historical(Hlc::from_parts(u64::MAX / 2, 0, 0), NOW));
    }

    /// The window is a tolerance, not a rounding error: a snapshot pinned
    /// at the top of a slow retrieval must still read as "latest".
    #[test]
    fn stale_but_in_window_pin_is_latest() {
        assert!(!is_historical(Hlc::from_parts(NOW - 30_000, 0, 0), NOW));
        assert!(!is_historical(Hlc::from_parts(NOW - AS_OF_LIVE_WINDOW_MS, 0, 0), NOW));
    }

    #[test]
    fn just_outside_the_window_is_historical() {
        assert!(is_historical(Hlc::from_parts(NOW - AS_OF_LIVE_WINDOW_MS - 1, 0, 0), NOW));
    }

    /// `wall_ms` near `u64::MAX` must not overflow into "historical".
    #[test]
    fn saturating_add_does_not_wrap() {
        assert!(!is_historical(Hlc::from_parts(u64::MAX, 0, 0), NOW));
    }

    #[test]
    fn reject_returns_the_typed_not_supported_variant() {
        let err = reject_historical_read(Hlc::from_parts(1_000, 0, 0))
            .expect_err("a 1970 pin must be refused");
        assert!(
            matches!(err, StorageError::NotSupported(_)),
            "must be the NotSupported variant (lunaris-server maps it to 501), got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("moon_kv_as_of"), "keep the greppable token: {msg}");
        assert!(msg.contains("read_as_of"), "name the operation: {msg}");
    }

    #[test]
    fn reject_passes_latest_reads_through() {
        assert!(reject_historical_read(Hlc::from_parts(now_millis(), 0, 0)).is_ok());
    }
}
