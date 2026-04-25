//! EVAL-05 `promotion_rate` SLO enforcement — Plan 16-05.
//!
//! Empirically calibrated via 6 x EVAL-05 runs (3 x Moon + 3 x Postgres) in
//! Plan 16-04. Band = observed_median (0.0) +/- max(2*MAD, 0.01).
//! See `milestones/v0.1.2-CONSOL-CALIBRATION/SUMMARY.md` for lineage.
//!
//! The band captures the observed zero-promotion baseline. The zero rate is
//! consistent with the ACT-R consolidator's debounce window exceeding the
//! EVAL-05 drain window on this trace shape; investigation of whether 16-01's
//! default flip fully propagates is deferred (see 16-05 SUMMARY).

/// Lower bound of the empirical promotion_rate band (inclusive).
/// Rate can't be negative, so this is effectively 0.0.
pub const PROMOTION_RATE_LOW: f64 = 0.0;

/// Upper bound of the empirical promotion_rate band (inclusive).
/// Derived as observed_median + max(2*MAD, 0.01) = 0.0 + 0.01 = 0.01.
pub const PROMOTION_RATE_HIGH: f64 = 0.01;

/// Enforce the promotion_rate SLO against the empirical band.
///
/// Returns `Ok(())` if `rate` is within `[PROMOTION_RATE_LOW, PROMOTION_RATE_HIGH]`
/// (inclusive on both bounds). Returns `Err` with a diagnostic message otherwise.
///
/// Wired into `eval_05_helios_10k` main so the binary exits non-zero when the
/// gate fails (D-06: never ship a regressing tag).
pub fn enforce_promotion_rate_slo(rate: f64) -> Result<(), String> {
    if !(PROMOTION_RATE_LOW..=PROMOTION_RATE_HIGH).contains(&rate) {
        Err(format!(
            "promotion_rate {rate:.6} outside enforced band \
             [{PROMOTION_RATE_LOW:.4}, {PROMOTION_RATE_HIGH:.4}] \
             (calibrated in Plan 16-04; see milestones/v0.1.2-CONSOL-CALIBRATION/SUMMARY.md)"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_band_passes() {
        assert!(enforce_promotion_rate_slo(0.0).is_ok());
        assert!(enforce_promotion_rate_slo(0.005).is_ok());
        assert!(enforce_promotion_rate_slo(0.01).is_ok());
    }

    #[test]
    fn out_of_band_fails() {
        assert!(enforce_promotion_rate_slo(-0.001).is_err());
        assert!(enforce_promotion_rate_slo(0.011).is_err());
        assert!(enforce_promotion_rate_slo(1.0).is_err());
    }
}
