//! Plan 16-05 Task 1 — promotion_rate SLO enforcement test.
//!
//! Tests `enforce_promotion_rate_slo` from `lunaris_bench::eval_05_slo` against
//! the empirical band derived in Plan 16-04 (6 x EVAL-05 runs, 3 x Moon + 3 x
//! Postgres). Band = [0.0, 0.01] (observed_median 0.0 +/- max(2*MAD, 0.01)).
//! See `milestones/v0.1.2-CONSOL-CALIBRATION/SUMMARY.md` for lineage.

use lunaris_bench::eval_05_slo::{enforce_promotion_rate_slo, PROMOTION_RATE_HIGH, PROMOTION_RATE_LOW};

#[test]
fn below_band_returns_err() {
    // rate = -0.001 is below PROMOTION_RATE_LOW (0.0)
    let result = enforce_promotion_rate_slo(-0.001);
    assert!(result.is_err(), "rate below band low must fail");
}

#[test]
fn above_band_returns_err() {
    // rate = 0.02 is above PROMOTION_RATE_HIGH (0.01)
    let result = enforce_promotion_rate_slo(0.02);
    assert!(result.is_err(), "rate above band high must fail");
}

#[test]
fn at_observed_median_returns_ok() {
    // observed_median = 0.0 is at PROMOTION_RATE_LOW (inclusive)
    let result = enforce_promotion_rate_slo(0.0);
    assert!(result.is_ok(), "rate at observed median (0.0) must pass");
}

#[test]
fn at_band_low_plus_epsilon_returns_ok() {
    // 0.0 + 1e-6 is safely inside the band
    let result = enforce_promotion_rate_slo(PROMOTION_RATE_LOW + 1e-6);
    assert!(result.is_ok(), "rate just above band low must pass");
}

#[test]
fn at_band_high_exactly_returns_ok() {
    // exactly at PROMOTION_RATE_HIGH (0.01) is inclusive — on the boundary
    let result = enforce_promotion_rate_slo(PROMOTION_RATE_HIGH);
    assert!(result.is_ok(), "rate at band high boundary must pass");
}

#[test]
fn at_band_high_plus_epsilon_returns_err() {
    let result = enforce_promotion_rate_slo(PROMOTION_RATE_HIGH + 1e-6);
    assert!(result.is_err(), "rate just above band high must fail");
}
