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

Any-gold mode (GA-2a recall ratchet, ``--anygold``):

  * Scores retrieval, not answers: was a gold-evidence session present in
    the reader context? Judge-free runs (``LUNARIS_EVAL_LME_JUDGE`` unset)
    emit NO ``LME_VERDICT`` line, so this mode reads the per-question debug
    trace ``=> evidence_recall_hit = true|false``
    (``LUNARIS_EVAL_LME_DEBUG=1``). When an ``LME_VERDICT`` line IS present
    (judge-mode artifacts), its ``evidence_recall_hit`` key wins and the
    judge's ``correct`` verdict is ignored: a wrong answer over retrieved
    gold evidence is still a retrieval hit.
  * ``--write-baseline FILE`` blesses a FINAL run as the checked-in ratchet
    baseline; ``--baseline FILE`` compares a FINAL run against one.
    ``--config-signature`` must accompany both — a baseline measured under
    one retrieval config must never gate another.

Exit codes: 0 ok / within tolerance; 1 ratchet REGRESSION beyond tolerance;
2 usage error (argparse); 5 run not final (coverage gap or ERR); 6 baseline
config-signature mismatch.

Usage:
    tally.py --dir <arm-dir> --expected <n>
    tally.py --dir <arm-dir> --expected <n> --json
    tally.py --dir <arm-dir> --expected <n> --anygold [--json]
    tally.py --dir <arm-dir> --expected <n> --anygold \
        --write-baseline <file> --config-signature <sig>
    tally.py --dir <arm-dir> --expected <n> --anygold \
        --baseline <file> --config-signature <sig>
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import platform
import re
import sys
import time

_Q_RE = re.compile(r"q(\d+)")
_ANYGOLD_RE = re.compile(r"evidence_recall_hit\s*=\s*(true|false)")

BASELINE_METRIC = "lme_s_anygold"
# One question of slack: an any-gold flip needs only one borderline rank to
# cross the session-capping boundary, and cross-platform float math (the
# baseline box vs the CI runner) can plausibly move one. Two is signal.
DEFAULT_TOLERANCE_QUESTIONS = 1


def offset_of(path: str) -> int:
    m = _Q_RE.search(os.path.basename(path))
    if m is None:  # pragma: no cover - filenames are produced by run_lme.sh
        raise ValueError(f"cannot parse question offset from {path}")
    return int(m.group(1))


def _last_verdict(text: str) -> dict | None:
    verdicts = [
        line.split(" ", 1)[1]
        for line in text.splitlines()
        if line.startswith("LME_VERDICT ")
    ]
    if not verdicts:
        return None
    try:
        v = json.loads(verdicts[-1])
    except ValueError:
        return None
    return v if isinstance(v, dict) else None


def classify(text: str) -> bool | None:
    """Return True (correct), False (wrong) or None (ERR) for one log body."""
    verdict = _last_verdict(text)
    if verdict is None or "correct" not in verdict or "judge error" in text:
        return None
    return bool(verdict["correct"])


def classify_anygold(text: str) -> bool | None:
    """True (gold evidence retrieved), False (missed) or None (ERR).

    Prefers the machine-readable ``LME_VERDICT`` payload; falls back to the
    last ``evidence_recall_hit = true|false`` debug trace. Note ``judge
    error`` does NOT poison this metric — retrieval completed regardless of
    what the judge did afterwards.
    """
    verdict = _last_verdict(text)
    if verdict is not None and "evidence_recall_hit" in verdict:
        return bool(verdict["evidence_recall_hit"])
    hits = _ANYGOLD_RE.findall(text)
    if hits:
        return hits[-1] == "true"
    return None


def tally(run_dir: str, anygold: bool = False) -> dict:
    files = sorted(glob.glob(os.path.join(run_dir, "q*.log")), key=offset_of)
    classifier = classify_anygold if anygold else classify
    correct: list[int] = []
    wrong: list[int] = []
    err: list[int] = []
    for path in files:
        off = offset_of(path)
        with open(path, errors="replace") as fh:
            verdict = classifier(fh.read())
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


def _fail(msg: str, code: int) -> int:
    print(f"FATAL: {msg}", file=sys.stderr)
    return code


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--dir", required=True, help="arm artifact directory")
    ap.add_argument("--expected", type=int, required=True, help="question count")
    ap.add_argument("--json", action="store_true", help="machine-readable output")
    ap.add_argument(
        "--anygold",
        action="store_true",
        help="score evidence_recall_hit (retrieval any-gold) instead of judge verdicts",
    )
    ap.add_argument("--baseline", help="ratchet: compare against this baseline JSON")
    ap.add_argument("--write-baseline", help="bless this FINAL run as the baseline JSON")
    ap.add_argument(
        "--config-signature",
        help="retrieval-config signature; required with --baseline/--write-baseline",
    )
    args = ap.parse_args(argv)

    if (args.baseline or args.write_baseline) and not args.config_signature:
        ap.error("--baseline/--write-baseline require --config-signature")
    if args.baseline and args.write_baseline:
        ap.error("--baseline and --write-baseline are mutually exclusive")
    if (args.baseline or args.write_baseline) and not args.anygold:
        ap.error("--baseline/--write-baseline are only defined for --anygold")

    result = tally(args.dir, anygold=args.anygold)
    scored = len(result["correct"]) + len(result["wrong"])
    final = result["artifacts"] == args.expected and not result["err"]
    result["scored"] = scored
    result["expected"] = args.expected
    result["final"] = final
    result["metric"] = "anygold" if args.anygold else "j"
    result["j"] = (100.0 * len(result["correct"]) / scored) if scored else None

    label = "any-gold" if args.anygold else "J"
    if args.json:
        print(json.dumps(result, indent=2, sort_keys=True))
    else:
        if scored:
            print(
                f"{label}(scored-only) = {result['j']:.1f}% "
                f"({len(result['correct'])}/{scored})  "
                f"{'miss' if args.anygold else 'wrong'}={result['wrong']}"
            )
        print(f"ERR({len(result['err'])}): {result['err']}")
        if final:
            print(
                f"RUN FINAL: {label} = "
                f"{100.0 * len(result['correct']) / args.expected:.1f}% "
                f"({len(result['correct'])}/{args.expected})"
            )
        else:
            print(
                f"RUN NOT FINAL: artifacts={result['artifacts']}/{args.expected}, "
                f"ERR={len(result['err'])} (final only at full coverage + ERR=0)"
            )

    if not (args.baseline or args.write_baseline):
        return 0

    # --- ratchet paths: both demand a FINAL run (the partial-arm lesson) ----
    hits = len(result["correct"])
    if not final:
        return _fail(
            f"run is NOT FINAL (artifacts={result['artifacts']}/{args.expected}, "
            f"ERR={len(result['err'])}: {result['err']}) — a partial run can "
            "neither be gated nor blessed as a baseline",
            5,
        )

    if args.write_baseline:
        baseline = {
            "metric": BASELINE_METRIC,
            "hits": hits,
            "total": args.expected,
            "tolerance_questions": DEFAULT_TOLERANCE_QUESTIONS,
            "config_signature": args.config_signature,
            "produced": {
                "date": time.strftime("%Y-%m-%d", time.gmtime()),
                "platform": f"{platform.system()}-{platform.machine()}",
            },
        }
        with open(args.write_baseline, "w") as fh:
            json.dump(baseline, fh, indent=2, sort_keys=True)
            fh.write("\n")
        print(
            f"BASELINE WRITTEN: {args.write_baseline} "
            f"(hits={hits}/{args.expected}, tolerance={DEFAULT_TOLERANCE_QUESTIONS}, "
            f"sig={args.config_signature})",
            file=sys.stderr,
        )
        return 0

    with open(args.baseline) as fh:
        base = json.load(fh)
    base_sig = base.get("config_signature")
    if base_sig != args.config_signature:
        return _fail(
            "baseline config-signature mismatch — this baseline was measured "
            "under a different retrieval config and cannot gate this run.\n"
            f"  baseline: {base_sig}\n"
            f"  this run: {args.config_signature}\n"
            "  Re-bless with --write-baseline under the new config if the "
            "change is intentional.",
            6,
        )
    if int(base.get("total", -1)) != args.expected:
        return _fail(
            f"baseline covers {base.get('total')} questions but this run "
            f"expected {args.expected} — offsets manifest drift. Re-bless the "
            "baseline for the new manifest.",
            6,
        )

    base_hits = int(base["hits"])
    tolerance = int(base.get("tolerance_questions", DEFAULT_TOLERANCE_QUESTIONS))
    floor = base_hits - tolerance
    verdict = f"any-gold {hits}/{args.expected} vs baseline {base_hits}/{base['total']} (tolerance {tolerance}, floor {floor})"
    if hits < floor:
        return _fail(
            f"any-gold REGRESSION: {verdict}. Retrieval quality dropped "
            "beyond tolerance — investigate before merging; re-bless the "
            "baseline only for an accepted, understood change.",
            1,
        )
    if hits > base_hits:
        print(
            f"RATCHET OK (above baseline): {verdict} — consider re-blessing "
            "to lock in the gain.",
            file=sys.stderr,
        )
    else:
        print(f"RATCHET OK: {verdict}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
