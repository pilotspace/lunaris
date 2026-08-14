#!/usr/bin/env python3
"""Three-way tally over a LongMemEval arm's per-question logs.

Extracted verbatim (behaviour-preserving) from the heredoc that used to live
inside the operator's run_lme.sh, so it can be unit-checked and reused by the
A/B comparison without copy-paste drift.

Scoring contract (expert-review H3/H5, 2026-07-28):

  * The ``LME_VERDICT <json>`` line is the SINGLE source of truth. The last
    one in a log wins.
  * A verdict without a ``correct`` key means the judge itself errored. That
    question is **ERR**, never "wrong" -- counting judge failures as wrong
    answers silently deflates J and was how a 20-question outage got mistaken
    for a 5-point regression.
  * A log containing ``judge error`` is ERR regardless of what it printed
    before.
  * A run is FINAL only at full offset coverage with ERR == 0. Anything else
    prints the scored-only number *and* a loud NOT FINAL banner, because a
    partial arm is not comparable to a complete one.

Usage:
    tally.py --dir <arm-dir> --expected <n>
    tally.py --dir <arm-dir> --expected <n> --json
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import sys

_Q_RE = re.compile(r"q(\d+)")


def offset_of(path: str) -> int:
    m = _Q_RE.search(os.path.basename(path))
    if m is None:  # pragma: no cover - filenames are produced by run_lme.sh
        raise ValueError(f"cannot parse question offset from {path}")
    return int(m.group(1))


def classify(text: str) -> bool | None:
    """Return True (correct), False (wrong) or None (ERR) for one log body."""
    verdicts = [
        line.split(" ", 1)[1]
        for line in text.splitlines()
        if line.startswith("LME_VERDICT ")
    ]
    verdict = None
    if verdicts:
        try:
            verdict = json.loads(verdicts[-1])
        except ValueError:
            verdict = None
    if verdict is None or "correct" not in verdict or "judge error" in text:
        return None
    return bool(verdict["correct"])


def tally(run_dir: str) -> dict:
    files = sorted(glob.glob(os.path.join(run_dir, "q*.log")), key=offset_of)
    correct: list[int] = []
    wrong: list[int] = []
    err: list[int] = []
    for path in files:
        off = offset_of(path)
        with open(path, errors="replace") as fh:
            verdict = classify(fh.read())
        if verdict is None:
            err.append(off)
        elif verdict:
            correct.append(off)
        else:
            wrong.append(off)
    return {
        "artifacts": len(files),
        "correct": correct,
        "wrong": wrong,
        "err": err,
    }


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--dir", required=True, help="arm artifact directory")
    ap.add_argument("--expected", type=int, required=True, help="question count")
    ap.add_argument("--json", action="store_true", help="machine-readable output")
    args = ap.parse_args(argv)

    result = tally(args.dir)
    scored = len(result["correct"]) + len(result["wrong"])
    final = result["artifacts"] == args.expected and not result["err"]
    result["scored"] = scored
    result["expected"] = args.expected
    result["final"] = final
    result["j"] = (100.0 * len(result["correct"]) / scored) if scored else None

    if args.json:
        print(json.dumps(result, indent=2, sort_keys=True))
        return 0

    if scored:
        print(
            f"J(scored-only) = {result['j']:.1f}% "
            f"({len(result['correct'])}/{scored})  wrong={result['wrong']}"
        )
    print(f"ERR({len(result['err'])}): {result['err']}")
    if final:
        print(
            f"RUN FINAL: J = {100.0 * len(result['correct']) / args.expected:.1f}% "
            f"({len(result['correct'])}/{args.expected})"
        )
    else:
        print(
            f"RUN NOT FINAL: artifacts={result['artifacts']}/{args.expected}, "
            f"ERR={len(result['err'])} (final only at full coverage + ERR=0)"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
