#!/usr/bin/env python3
"""publish_raw.py — turn a finished benchmark arm into a COMMITTABLE raw envelope.

The rule this script exists to enforce (docs/benchmarks/README.md):

    A published number without a committed raw artifact is not publishable.

GA-2b already does this correctly by hand (docs/benchmarks/ga2b-raw/*.json).
LongMemEval and PersonaMem do not: both default their artifacts to `target/`,
which is gitignored, so every number they have ever produced died with the
operator's working tree. That is exactly how the `85.4% (427/500)` headline
became unreproducible and had to be retracted.

What it does
------------
1. Scores the arm directory with the benchmark's own `tally.py` (--json).
2. REFUSES a run that is not FINAL (incomplete coverage, or any ERR). A
   partial arm can be neither gated nor published.
3. Reads the arm's `config.env` for the retrieval-config signature the
   harness itself computed, so the envelope cannot describe a different
   config than the one that ran.
4. REFUSES an `--operating-point` that contradicts the measured rerank
   setting. `fast` means rerank OFF, `quality` means rerank ON; mislabelling
   is the defect the two-point decision exists to kill, so it is an error,
   not a warning.
5. Stamps commit SHA, working-tree cleanliness, UTC date and platform.
6. Writes the envelope where it will be committed, and prints the path.

It deliberately does NOT compute or adjust any score: every number in the
envelope comes from the harness's own tally output.

Usage
-----
    scripts/bench/publish_raw.py --benchmark lme \\
        --dir target/lme/graphoff --expected 125 \\
        --operating-point quality --arm graphoff \\
        --out docs/benchmarks/lme-raw/2026-08-21-n125-graphoff-quality.json

    scripts/bench/publish_raw.py --benchmark lme --anygold \\
        --dir target/lme/anygold --expected 40 \\
        --operating-point fast --arm ci-ratchet \\
        --out docs/benchmarks/lme-raw/2026-08-21-anygold-n40-fast.json

    scripts/bench/publish_raw.py --benchmark pm \\
        --dir target/pm/32k-memory --expected 37 \\
        --operating-point quality --arm memory-sonnet5 \\
        --out docs/benchmarks/pm-raw/2026-08-21-32k-memory-quality.json

Exit codes: 0 written / 2 bad usage / 5 run not final / 6 operating-point
contradicts the measured config / 7 harness or repo state unusable.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
import sys
import time

SCHEMA = "lunaris-bench-raw/1"
HERE = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))

BENCHMARKS = {
    "lme": {
        "dataset": "longmemeval_s",
        "tally": os.path.join(HERE, "lme", "tally.py"),
        "runner": "scripts/bench/lme/run_lme.sh (or anygold_gate.sh)",
        "rerank_key": "LUNARIS_EVAL_LME_RERANK",
        "expected_unit": "questions",
    },
    "pm": {
        "dataset": "personamem",
        "tally": os.path.join(HERE, "pm", "tally.py"),
        "runner": "scripts/bench/pm/run_pm.sh",
        "rerank_key": "LUNARIS_EVAL_PM_RERANK",
        "expected_unit": "shared contexts",
    },
}

TRUTHY = {"1", "true", "TRUE", "on", "ON"}


def die(msg: str, code: int) -> "None":
    print(f"FATAL: {msg}", file=sys.stderr)
    raise SystemExit(code)


def read_config_env(run_dir: str) -> tuple[str | None, dict[str, str]]:
    """Return (signature, env-map) from the arm's config.env.

    LME writes a leading ``SIG=<signature>`` line; PersonaMem writes only the
    env pairs. When there is no SIG line we synthesise a deterministic one
    from the eval-facing variables, so a PersonaMem envelope still carries a
    comparable fingerprint.
    """
    path = os.path.join(run_dir, "config.env")
    if not os.path.isfile(path):
        die(
            f"{path} not found — this directory was not produced by a harness "
            "runner, or the run predates config fingerprinting. An envelope "
            "must describe a config the harness itself recorded.",
            7,
        )
    sig = None
    env: dict[str, str] = {}
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            key, _, value = line.partition("=")
            if key == "SIG":
                sig = value
            else:
                env[key] = value
    if sig is None:
        eval_vars = sorted(k for k in env if k.startswith("LUNARIS_EVAL_"))
        sig = "derived-v1|" + "|".join(f"{k}={env[k]}" for k in eval_vars)
    return sig, env


def rerank_state(sig: str, env: dict[str, str], rerank_key: str) -> bool | None:
    """True = rerank ON, False = OFF, None = the run did not record it."""
    if rerank_key in env:
        return env[rerank_key] in TRUTHY
    for part in sig.split("|"):
        if part.startswith("rerank="):
            return part.split("=", 1)[1] in TRUTHY
    return None


def git(*args: str) -> str | None:
    try:
        out = subprocess.run(
            ["git", "-C", REPO_ROOT, *args],
            capture_output=True,
            text=True,
            check=True,
        )
        return out.stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--benchmark", required=True, choices=sorted(BENCHMARKS))
    ap.add_argument("--dir", required=True, help="the finished arm artifact directory")
    ap.add_argument("--expected", type=int, required=True, help="expected coverage (see the benchmark's tally.py)")
    ap.add_argument(
        "--operating-point",
        required=True,
        choices=("fast", "quality"),
        help="fast = rerank OFF (shipped default); quality = rerank ON. "
        "Checked against the measured config; a mismatch is an error.",
    )
    ap.add_argument("--out", required=True, help="envelope path under docs/benchmarks/<bench>-raw/")
    ap.add_argument("--arm", default=None, help="short arm label, e.g. graphoff / memory-sonnet5")
    ap.add_argument("--anygold", action="store_true", help="LME only: score retrieval any-gold instead of judge verdicts")
    ap.add_argument("--note", default=None, help="free-text note stored in the envelope")
    ap.add_argument("--dry-run", action="store_true", help="print the envelope, write nothing")
    args = ap.parse_args(argv)

    spec = BENCHMARKS[args.benchmark]
    if args.anygold and args.benchmark != "lme":
        ap.error("--anygold is only defined for --benchmark lme")
    if not os.path.isdir(args.dir):
        die(f"arm directory not found: {args.dir}", 2)

    tally_argv = [sys.executable, spec["tally"], "--dir", args.dir, "--expected", str(args.expected), "--json"]
    if args.anygold:
        tally_argv.append("--anygold")
    proc = subprocess.run(tally_argv, capture_output=True, text=True)
    if proc.returncode != 0 or not proc.stdout.strip():
        die(
            f"tally failed (rc={proc.returncode}) — nothing to publish.\n{proc.stderr.strip()}",
            7,
        )
    tally = json.loads(proc.stdout)

    if not tally.get("final"):
        die(
            "run is NOT FINAL (incomplete coverage or ERR > 0). A partial arm is "
            "not comparable to a complete one and must not be published. "
            f"tally: {json.dumps({k: tally.get(k) for k in ('artifacts', 'expected', 'final')})}",
            5,
        )

    sig, env = read_config_env(args.dir)
    measured = rerank_state(sig, env, spec["rerank_key"])
    if measured is None:
        die(
            "the run did not record its rerank setting, so its operating point "
            "cannot be verified. Re-run with a harness version that writes "
            f"{spec['rerank_key']} into config.env rather than labelling by hand.",
            6,
        )
    declared_quality = args.operating_point == "quality"
    if measured != declared_quality:
        die(
            f"--operating-point {args.operating_point} contradicts the measured "
            f"config (rerank {'ON' if measured else 'OFF'}). "
            f"{'quality' if measured else 'fast'} is what ran. "
            "Mislabelling an operating point is the defect this check exists "
            "to prevent — see docs/benchmarks/operating-points.md.",
            6,
        )

    head = git("rev-parse", "HEAD")
    dirty = git("status", "--porcelain")
    envelope = {
        "schema": SCHEMA,
        "benchmark": spec["dataset"],
        "arm": args.arm or os.path.basename(os.path.normpath(args.dir)),
        "operating_point": args.operating_point,
        "config_signature": sig,
        "metric": tally.get("metric", "accuracy"),
        "expected_unit": spec["expected_unit"],
        "tally": tally,
        "harness": {
            "runner": spec["runner"],
            "tally": os.path.relpath(spec["tally"], REPO_ROOT),
            "publisher": "scripts/bench/publish_raw.py",
        },
        "provenance": {
            "commit_sha": head,
            "working_tree_clean": (dirty == "") if dirty is not None else None,
            "run_date_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "platform": f"{platform.system()}-{platform.machine()}",
            "artifact_dir": args.dir,
        },
        "note": args.note,
    }

    text = json.dumps(envelope, indent=2, sort_keys=True) + "\n"
    if args.dry_run:
        print(text, end="")
        return 0
    out_dir = os.path.dirname(os.path.abspath(args.out))
    os.makedirs(out_dir, exist_ok=True)
    with open(args.out, "w") as fh:
        fh.write(text)
    print(f"ENVELOPE WRITTEN: {args.out}", file=sys.stderr)
    print(text, end="")
    if envelope["provenance"]["working_tree_clean"] is False:
        print(
            "WARNING: the working tree was dirty at publish time. The commit SHA "
            "in this envelope does not fully describe what ran.",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
