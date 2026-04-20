//! Helpers that map a `WriteOp` / read key to a Postgres table + column set.
//!
//! `Hlc → TIMESTAMPTZ + BIGINT` packing convention is locked here:
//!
//!   `valid_from_hlc = (wall_ms << 32) | counter`  (BIGINT)
//!   `valid_from`    = `to_timestamp(wall_ms / 1000.0 + counter * 1e-6)`  (TIMESTAMPTZ)
//!
//! Both columns are populated on every write so AS_OF queries can use whichever ordering
//! granularity the caller needs. Phase 1 reads use TIMESTAMPTZ; Phase 5 conformance uses
//! BIGINT for byte-identical AS_OF parity vs Moon.

use chrono::{DateTime, Utc};
use lunaris_core::bitemporal::BiTemporal;
use lunaris_core::hlc::Hlc;

#[inline]
pub fn hlc_to_ts(h: Hlc) -> DateTime<Utc> {
    let secs = (h.wall_ms / 1000) as i64;
    // sub-millisecond ordering — pack the HLC counter into the nanosecond field.
    let sub_ms_nanos = ((h.wall_ms % 1000) * 1_000_000) as u32;
    let nanos = sub_ms_nanos.saturating_add(h.counter);
    DateTime::from_timestamp(secs, nanos).unwrap_or_else(Utc::now)
}

#[inline]
pub fn hlc_packed(h: Hlc) -> i64 {
    ((h.wall_ms as i64) << 32) | (h.counter as i64)
}

/// Decompose a BiTemporal into the eight (valid_from_ts, valid_to_ts, sys_from_ts, sys_to_ts,
/// valid_from_hlc, valid_to_hlc-or-NULL, sys_from_hlc, sys_to_hlc-or-NULL) values bound to SQL.
pub struct BtRow {
    pub valid_from_ts: DateTime<Utc>,
    pub valid_to_ts: Option<DateTime<Utc>>,
    pub sys_from_ts: DateTime<Utc>,
    pub sys_to_ts: Option<DateTime<Utc>>,
    pub valid_from_hlc: i64,
    pub sys_from_hlc: i64,
}

impl From<&BiTemporal> for BtRow {
    fn from(b: &BiTemporal) -> Self {
        Self {
            valid_from_ts: hlc_to_ts(b.valid.0),
            valid_to_ts: b.valid.1.map(hlc_to_ts),
            sys_from_ts: hlc_to_ts(b.sys.0),
            sys_to_ts: b.sys.1.map(hlc_to_ts),
            valid_from_hlc: hlc_packed(b.valid.0),
            sys_from_hlc: hlc_packed(b.sys.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::hlc::Hlc;

    #[test]
    fn hlc_packed_layout() {
        let h = Hlc { wall_ms: 0x1234_5678, counter: 0xABCD, node_id: 0 };
        let packed = hlc_packed(h);
        // Top 32 bits = wall_ms, bottom 32 = counter.
        assert_eq!(packed, (0x1234_5678i64 << 32) | 0xABCDi64);
    }

    #[test]
    fn hlc_to_ts_does_not_panic_for_zero() {
        let _ = hlc_to_ts(Hlc::ZERO);
    }
}
