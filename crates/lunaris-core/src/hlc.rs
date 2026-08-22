//! Hybrid Logical Clock — wall-time millis + monotonic counter + node id.
//!
//! Issued timestamps are totally ordered across threads; under contention the
//! counter increments instead of the wall clock so we never return duplicates.
//!
//! Storage layout (24 bytes):
//!   wall_ms : u64  — unix millis at last advance
//!   counter : u32  — incremented when wall did not advance
//!   node_id : u16  — process / node identity (0 in single-node v0)
//!   _pad    : u16
//!
//! Total order: (wall_ms, counter, node_id) lex comparison.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hlc {
    pub wall_ms: u64,
    pub counter: u32,
    pub node_id: u16,
}

impl Hlc {
    pub const ZERO: Hlc = Hlc { wall_ms: 0, counter: 0, node_id: 0 };

    pub fn from_parts(wall_ms: u64, counter: u32, node_id: u16) -> Self {
        Self { wall_ms, counter, node_id }
    }

    /// An `Hlc` addressing a real-world instant, for use on the **valid**
    /// axis of a [`crate::BiTemporal`].
    ///
    /// This is deliberately NOT issued by an [`HlcClock`]. A clock-issued
    /// stamp answers "when did this process observe the event", which is the
    /// **system** axis; the valid axis answers "when was this true in the
    /// world" and routinely points into the past. Backdating the system axis
    /// would break the total order the clock exists to guarantee — backdating
    /// the valid axis is the whole point of having two.
    ///
    /// `counter` and `node_id` are zero: a real-world instant carries no
    /// causality, so there is no tie to break. Two episodes stamped at the
    /// same real-world millisecond compare equal on the valid axis, which is
    /// the correct answer.
    ///
    /// Pre-epoch instants clamp to zero. `wall_ms` is unsigned, so the
    /// alternative is a wrap into the far future — a document dated 1969
    /// would sort after everything instead of before it.
    pub fn from_utc(t: chrono::DateTime<chrono::Utc>) -> Self {
        Self { wall_ms: t.timestamp_millis().max(0) as u64, counter: 0, node_id: 0 }
    }
}

#[derive(Debug)]
pub struct HlcClock {
    inner: Mutex<Hlc>,
    node_id: u16,
}

impl HlcClock {
    pub fn new(node_id: u16) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Hlc { wall_ms: now_millis(), counter: 0, node_id }),
            node_id,
        })
    }

    /// Issue the next monotonic timestamp.
    ///
    /// Algorithm:
    ///   wall = max(prev.wall_ms, system_now)
    ///   if wall == prev.wall_ms: counter = prev.counter + 1
    ///   else: counter = 0
    ///
    /// The lock is held only across pure CPU work — never across .await.
    pub fn tick(&self) -> Hlc {
        let mut g = self.inner.lock();
        let now = now_millis();
        if now > g.wall_ms {
            g.wall_ms = now;
            g.counter = 0;
        } else {
            // wall did not advance — bump counter
            g.counter = g.counter.saturating_add(1);
        }
        let issued = *g;
        Hlc { wall_ms: issued.wall_ms, counter: issued.counter, node_id: self.node_id }
    }

    pub fn node_id(&self) -> u16 {
        self.node_id
    }
}

#[inline]
fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
