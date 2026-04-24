#!/usr/bin/env python3
"""
scripts/16-04-compute-band.py — Plan 16-04 Task 2

Stdlib-only computation of the CONSOL-V1-02 enforcement band from the 6 EVAL-05
calibration runs emitted by `scripts/16-04-calibrate.sh`.

Inputs
------
  milestones/v0.1.2-CONSOL-CALIBRATION/run-moon-{1,2,3}.json
  milestones/v0.1.2-CONSOL-CALIBRATION/run-postgres-{1,2,3}.json

Output
------
  milestones/v0.1.2-CONSOL-CALIBRATION/SUMMARY.md

Guards (all three must pass; any flag -> SUMMARY disposition `checkpoint:decision`)
  1. Fingerprint-identity (R16-01): all 6 runs share identical {git_sha,
     moon_version, pg_lunaris_image_digest, candle_cache_hash, trace_hash}.
  2. Cross-backend divergence (P16-D05): |median_moon - median_pg| <= 2*MAD.
  3. Placeholder-band sanity (R16-02): band stays inside REQUIREMENTS.md
     placeholder [0.02, 0.15].

Band arithmetic
---------------
  observed_median = median(promotion_rate over 6 runs)
  mad             = median(|v - observed_median|)
  half_band       = max(2 * mad, 0.01)   # P16-D03 floor
  band_low        = observed_median - half_band
  band_high       = observed_median + half_band

Exit codes
----------
  0 — SUMMARY written; disposition may be `proceed` or `checkpoint:decision`
  2 — fingerprint drift detected; SUMMARY still written (flagged)
  3 — insufficient / malformed run-*.json; nothing written
"""

from __future__ import annotations

import json
import os
import statistics
import sys
from pathlib import Path
from typing import Any


CALIB_DIR = Path("milestones/v0.1.2-CONSOL-CALIBRATION")

EXPECTED_RUN_IDS: list[str] = [
    "moon-1", "moon-2", "moon-3",
    "postgres-1", "postgres-2", "postgres-3",
]

FINGERPRINT_FIELDS: list[str] = [
    "git_sha",
    "moon_version",
    "pg_lunaris_image_digest",
    "candle_cache_hash",
    "trace_hash",
]

# REQUIREMENTS.md CONSOL-V1-02 placeholder band
PLACEHOLDER_LOW: float = 0.02
PLACEHOLDER_HIGH: float = 0.15

# P16-D03 half-band floor (guards against degenerate MAD=0 on identical runs)
HALF_BAND_FLOOR: float = 0.01


def load_runs() -> list[dict[str, Any]]:
    runs: list[dict[str, Any]] = []
    missing: list[str] = []
    for rid in EXPECTED_RUN_IDS:
        p = CALIB_DIR / f"run-{rid}.json"
        if not p.is_file():
            missing.append(str(p))
            continue
        try:
            with p.open() as f:
                runs.append(json.load(f))
        except json.JSONDecodeError as e:
            print(f"error: {p} is not valid JSON: {e}", file=sys.stderr)
            sys.exit(3)
    if missing:
        print("error: missing calibration runs:", file=sys.stderr)
        for m in missing:
            print(f"  - {m}", file=sys.stderr)
        print("hint: run scripts/16-04-calibrate.sh first.", file=sys.stderr)
        sys.exit(3)
    return runs


def check_fingerprint(runs: list[dict[str, Any]]) -> tuple[bool, list[str]]:
    """Return (ok, diff_lines). ok=False iff any field drifts across runs."""
    diffs: list[str] = []
    baseline = {k: runs[0].get(k) for k in FINGERPRINT_FIELDS}
    for r in runs[1:]:
        for k in FINGERPRINT_FIELDS:
            if r.get(k) != baseline[k]:
                diffs.append(
                    f"  {r.get('run_id', '?')}.{k} = {r.get(k)!r} "
                    f"(baseline: {baseline[k]!r})"
                )
    return (len(diffs) == 0, diffs)


def extract_rates(runs: list[dict[str, Any]]) -> list[float]:
    rates: list[float] = []
    for r in runs:
        v = r.get("promotion_rate")
        if v is None:
            print(
                f"error: run {r.get('run_id')} has no promotion_rate field.",
                file=sys.stderr,
            )
            sys.exit(3)
        rates.append(float(v))
    return rates


def compute_band(values: list[float]) -> dict[str, float]:
    median = statistics.median(values)
    mad = statistics.median([abs(v - median) for v in values])
    half_band = max(2.0 * mad, HALF_BAND_FLOOR)
    return {
        "observed_median": median,
        "mad": mad,
        "half_band": half_band,
        "band_low": median - half_band,
        "band_high": median + half_band,
    }


def format_float(x: float | None, fmt: str = ".4f") -> str:
    if x is None:
        return "—"
    return format(x, fmt)


def write_summary(
    runs: list[dict[str, Any]],
    rates: list[float],
    band: dict[str, float],
    fingerprint_ok: bool,
    fingerprint_diffs: list[str],
    cross_backend: dict[str, Any],
    placeholder_exceeded: bool,
) -> Path:
    baseline = {k: runs[0].get(k) for k in FINGERPRINT_FIELDS}

    flags: list[str] = []
    if not fingerprint_ok:
        flags.append("FINGERPRINT_DRIFT")
    if cross_backend["diverged"]:
        flags.append("CROSS_BACKEND_DIVERGENCE")
    if placeholder_exceeded:
        flags.append("PLACEHOLDER_BAND_EXCEEDED")
    if not flags:
        flags.append("OK")

    disposition = (
        "proceed to 16-05"
        if flags == ["OK"]
        else "checkpoint:decision required before 16-05"
    )

    lines: list[str] = []
    lines.append("# CONSOL-V1-02 Calibration SUMMARY (Plan 16-04)")
    lines.append("")
    lines.append(f"Generated: {runs[0].get('captured_at', '?')} (first run captured_at)")
    lines.append("")
    lines.append(f"**Disposition:** {disposition}")
    lines.append(f"**Flags:** {', '.join(flags)}")
    lines.append("")

    # Runs table
    lines.append("## Observed runs")
    lines.append("")
    lines.append("| run_id | backend | promotion_rate | eval_05_p50_ms | queue_depth_peak |")
    lines.append("| --- | --- | --- | --- | --- |")
    for r in runs:
        lines.append(
            "| {rid} | {be} | {pr} | {p50} | {qd} |".format(
                rid=r.get("run_id", "?"),
                be=r.get("backend", "?"),
                pr=format_float(r.get("promotion_rate")),
                p50=format_float(r.get("eval_05_p50_ms"), ".2f"),
                qd=r.get("queue_depth_peak", "—"),
            )
        )
    lines.append("")

    # Fingerprint
    lines.append("## Env fingerprint (asserted identical across all 6 runs)")
    lines.append("")
    if fingerprint_ok:
        lines.append("_All 6 runs share the following fingerprint:_")
        lines.append("")
        for k in FINGERPRINT_FIELDS:
            lines.append(f"- `{k}` = `{baseline[k]}`")
    else:
        lines.append("**FINGERPRINT DRIFT DETECTED — band computation is UNTRUSTED.**")
        lines.append("")
        lines.append("Drifting fields:")
        for d in fingerprint_diffs:
            lines.append(f"- {d}")
        lines.append("")
        lines.append("Resolution: re-run `scripts/16-04-calibrate.sh --force` on a clean box.")
    lines.append("")

    # Band
    lines.append("## Derived band")
    lines.append("")
    lines.append(f"- `observed_median` = **{band['observed_median']:.4f}**")
    lines.append(f"- `mad`             = {band['mad']:.4f}")
    lines.append(
        f"- `half_band`       = max(2×MAD, {HALF_BAND_FLOOR}) "
        f"= **{band['half_band']:.4f}**"
    )
    lines.append(
        f"- **band** = `[{band['band_low']:.4f}, {band['band_high']:.4f}]` "
        f"(observed_median ± half_band)"
    )
    lines.append("")

    # Cross-backend
    lines.append("## Cross-backend divergence (P16-D05)")
    lines.append("")
    lines.append(f"- moon median      = {cross_backend['moon_median']:.4f}")
    lines.append(f"- postgres median  = {cross_backend['pg_median']:.4f}")
    lines.append(f"- |Δ|              = {cross_backend['delta']:.4f}")
    lines.append(f"- threshold (2·MAD) = {cross_backend['threshold']:.4f}")
    if cross_backend["diverged"]:
        lines.append("")
        lines.append(
            "**CROSS_BACKEND_DIVERGENCE**: backends diverge by more than one "
            "band-width. Do NOT silently switch to per-backend bands — escalate "
            "to the operator for a `checkpoint:decision`."
        )
    lines.append("")

    # Placeholder sanity
    lines.append("## Placeholder-band sanity (R16-02)")
    lines.append("")
    lines.append(
        f"- REQUIREMENTS.md CONSOL-V1-02 placeholder = "
        f"`[{PLACEHOLDER_LOW}, {PLACEHOLDER_HIGH}]`"
    )
    if placeholder_exceeded:
        lines.append(
            "- **PLACEHOLDER_BAND_EXCEEDED**: observed band falls outside the "
            "placeholder. 16-05 MUST NOT silently commit — operator must "
            "choose {widen-band, investigate, override}."
        )
    else:
        lines.append("- Observed band fits inside the placeholder. ✓")
    lines.append("")

    # Hand-off
    lines.append("## Hand-off to Plan 16-05")
    lines.append("")
    if flags == ["OK"]:
        lines.append(
            f"16-05 shall commit band "
            f"**`[{band['band_low']:.4f}, {band['band_high']:.4f}]`** "
            f"to REQUIREMENTS.md CONSOL-V1-02 and to the EVAL-05 gate constants."
        )
    else:
        lines.append(
            "16-05 is BLOCKED until the operator resolves the flagged "
            "`checkpoint:decision` (one of: proceed / widen-band / "
            "investigate / override)."
        )
    lines.append("")

    out = CALIB_DIR / "SUMMARY.md"
    out.write_text("\n".join(lines))
    return out


def main() -> int:
    if not CALIB_DIR.is_dir():
        print(f"error: {CALIB_DIR} does not exist.", file=sys.stderr)
        return 3

    runs = load_runs()

    fingerprint_ok, fingerprint_diffs = check_fingerprint(runs)

    rates = extract_rates(runs)
    band = compute_band(rates)

    # Cross-backend divergence
    moon_rates = [float(r["promotion_rate"]) for r in runs if r["backend"] == "moon"]
    pg_rates = [float(r["promotion_rate"]) for r in runs if r["backend"] == "postgres"]
    moon_median = statistics.median(moon_rates) if moon_rates else 0.0
    pg_median = statistics.median(pg_rates) if pg_rates else 0.0
    delta = abs(moon_median - pg_median)
    threshold = 2.0 * band["mad"]
    cross_backend = {
        "moon_median": moon_median,
        "pg_median": pg_median,
        "delta": delta,
        "threshold": threshold,
        "diverged": delta > threshold,
    }

    # Placeholder sanity
    placeholder_exceeded = (
        band["band_low"] < PLACEHOLDER_LOW or band["band_high"] > PLACEHOLDER_HIGH
    )

    out_path = write_summary(
        runs=runs,
        rates=rates,
        band=band,
        fingerprint_ok=fingerprint_ok,
        fingerprint_diffs=fingerprint_diffs,
        cross_backend=cross_backend,
        placeholder_exceeded=placeholder_exceeded,
    )
    print(f"wrote {out_path}")

    if not fingerprint_ok:
        print(
            "warning: fingerprint drift detected — SUMMARY.md flags FINGERPRINT_DRIFT.",
            file=sys.stderr,
        )
        return 2

    return 0


if __name__ == "__main__":
    sys.exit(main())
