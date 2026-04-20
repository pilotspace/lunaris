//! Bi-temporal validity stamp per blueprint §3.3.
//!
//!   valid : when the fact is true in the world
//!   sys   : when the system observed/recorded the fact
//!
//! Half-open intervals: [from, to). `to = None` means "still valid".

use serde::{Deserialize, Serialize};

use crate::hlc::{Hlc, HlcClock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BiTemporal {
    pub valid: (Hlc, Option<Hlc>),
    pub sys: (Hlc, Option<Hlc>),
}

impl BiTemporal {
    /// New record stamped with `clock.tick()` for both valid_from and sys_from.
    pub fn now(clock: &HlcClock) -> Self {
        let t = clock.tick();
        Self { valid: (t, None), sys: (t, None) }
    }

    pub fn at(valid_from: Hlc, sys_from: Hlc) -> Self {
        Self { valid: (valid_from, None), sys: (sys_from, None) }
    }

    pub fn invalidate_valid(&mut self, t: Hlc) {
        self.valid.1 = Some(t);
    }
    pub fn invalidate_sys(&mut self, t: Hlc) {
        self.sys.1 = Some(t);
    }

    /// Was the fact true in the world at `t`?
    pub fn valid_at(&self, t: Hlc) -> bool {
        self.valid.0 <= t && self.valid.1.is_none_or(|end| t < end)
    }
    /// Was the fact recorded in the system at `t`?
    pub fn system_at(&self, t: Hlc) -> bool {
        self.sys.0 <= t && self.sys.1.is_none_or(|end| t < end)
    }
    /// AS_OF query: both axes hold at `t`.
    pub fn overlaps(&self, t: Hlc) -> bool {
        self.valid_at(t) && self.system_at(t)
    }
}
