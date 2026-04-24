# CONSOL-V1-02 Calibration SUMMARY (Plan 16-04)

Generated: 2026-04-24T04:10:52Z (first run captured_at)

**Disposition:** checkpoint:decision required before 16-05
**Flags:** PLACEHOLDER_BAND_EXCEEDED

## Observed runs

| run_id | backend | promotion_rate | eval_05_p50_ms | queue_depth_peak |
| --- | --- | --- | --- | --- |
| moon-1 | moon | 0.0000 | 2.71 | None |
| moon-2 | moon | 0.0000 | 3.45 | None |
| moon-3 | moon | 0.0000 | 4.72 | None |
| postgres-1 | postgres | 0.0000 | 3.60 | None |
| postgres-2 | postgres | 0.0000 | 4.77 | None |
| postgres-3 | postgres | 0.0000 | 5.01 | None |

## Env fingerprint (asserted identical across all 6 runs)

_All 6 runs share the following fingerprint:_

- `git_sha` = `8a5db5d7c979504402c4c235c9fe3013022c8faf`
- `moon_version` = `redis-compat:0.1.0`
- `pg_lunaris_image_digest` = `sha256:67b40fab19417f3777ac1047f0dae077e27e2032f3756b71b5121e6aebb14999`
- `candle_cache_hash` = `none`
- `trace_hash` = `a4b024082b67cb26c98af53587b62d9e64ee8304bc97dab746f6342249f24cd2`

## Derived band

- `observed_median` = **0.0000**
- `mad`             = 0.0000
- `half_band`       = max(2×MAD, 0.01) = **0.0100**
- **band** = `[-0.0100, 0.0100]` (observed_median ± half_band)

## Cross-backend divergence (P16-D05)

- moon median      = 0.0000
- postgres median  = 0.0000
- |Δ|              = 0.0000
- threshold (2·MAD) = 0.0000

## Placeholder-band sanity (R16-02)

- REQUIREMENTS.md CONSOL-V1-02 placeholder = `[0.02, 0.15]`
- **PLACEHOLDER_BAND_EXCEEDED**: observed band falls outside the placeholder. 16-05 MUST NOT silently commit — operator must choose {widen-band, investigate, override}.

## Hand-off to Plan 16-05

16-05 is BLOCKED until the operator resolves the flagged `checkpoint:decision` (one of: proceed / widen-band / investigate / override).
