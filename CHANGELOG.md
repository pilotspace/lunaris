# Changelog

All notable changes to Lunaris are documented here.

## v0.1.2

### Changed

- **BREAKING-LIKE (behavioral default change)**: `ConsolidatorPipelineHandle::default()` now
  wires `ActRConsolidator` instead of `NoopConsolidator`. The three-surface toggle (code / env /
  config) is preserved. To retain the v0.1.1 behavior, set `LUNARIS_CONSOLIDATOR_BACKEND=noop`
  before `Lunaris::open`, or call
  `handle.consolidator_pipeline().set_consolidator(Arc::new(NoopConsolidator))` explicitly.
  See `docs/migration/v0.1.2.md` and Phase 16 plans.
- EVAL-05 `promotion_rate` SLO is now enforced (was informational in v0.1.1), with empirical
  band [0.00, 0.01] calibrated against the deterministic 10K-turn trace on Moon + Postgres
  (6 runs: 3 x Moon + 3 x Postgres; see `milestones/v0.1.2-CONSOL-CALIBRATION/SUMMARY.md`).

### Added

- `lunaris_bench::eval_05_slo` module with `enforce_promotion_rate_slo()` function and
  `PROMOTION_RATE_LOW` / `PROMOTION_RATE_HIGH` constants for CI enforcement.
- `docs/migration/v0.1.2.md` migration guide for downstream consumers.
- `milestones/v0.1.2-CONSOL-CALIBRATION/` with 6-run calibration artifacts and band derivation.

## v0.1.1

Released 2026-04-23. See `milestones/v0.1.1-MILESTONE-AUDIT.md` for full details.

## v0.1.0

Released 2026-04-21. See `milestones/v0.1.0-MILESTONE-AUDIT.md` for full details.
