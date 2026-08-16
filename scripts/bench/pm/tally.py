#!/usr/bin/env python3
"""Three-way tally over a PersonaMem arm's per-context logs.

Mirrors ``scripts/bench/lme/tally.py`` (same H3/H5 scoring contract), adapted
to PersonaMem's unit of work: one log file per SHARED CONTEXT, carrying one
``PM_VERDICT <json>`` line per question inside it.

Scoring contract:

  * The ``PM_VERDICT <json>`` lines are the SINGLE source of truth. Within one
    log, the LAST verdict for a given ``question_id`` wins (a retried attempt
    appends, it does not truncate).
  * A verdict carrying an ``error`` key is **ERR**, never "wrong" -- counting a
    chat/transport failure as a wrong answer silently deflates accuracy.
  * A verdict whose ``predicted`` is null but which carries NO error is a real
    WRONG: the reader replied something with no parseable option letter.
  * A run is FINAL only at full context coverage with ERR == 0. Anything else
    prints the scored-only number *and* a loud NOT FINAL banner, because a
    partial arm is not comparable to a complete one -- and PersonaMem's
    accuracy is only claimable against Tencent's 76% at full coverage.

Usage:
    tally.py --dir <arm-dir> --expected <n-contexts>
    tally.py --dir <arm-dir> --expected <n-contexts> --json
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import sys
from collections import OrderedDict, defaultdict

_C_RE = re.compile(r"c(\d+)")

TENCENT_WITH_MEMORY = 76.0
TENCENT_WITHOUT_MEMORY = 48.0


def offset_of(path: str) -> int:
    m = _C_RE.search(os.path.basename(path))
    if m is None:  # pragma: no cover - filenames are produced by run_pm.sh
        raise ValueError(f"cannot parse context offset from {path}")
    return int(m.group(1))


def verdicts_in(text: str) -> "OrderedDict[str, dict]":
    """Last PM_VERDICT per question_id, in first-seen order."""
    out: OrderedDict[str, dict] = OrderedDict()
    for line in text.splitlines():
        if not line.startswith("PM_VERDICT "):
            continue
        try:
            v = json.loads(line.split(" ", 1)[1])
        except ValueError:
            continue
        qid = v.get("question_id")
        if not qid:
            continue
        if qid in out:
            out[qid] = v          # keep position, take the newer verdict
        else:
            out[qid] = v
    return out


def classify(v: dict) -> bool | None:
    """True (correct), False (wrong) or None (ERR) for one verdict."""
    if v.get("error"):
        return None
    if "correct" not in v:
        return None
    return bool(v["correct"])


def tally(run_dir: str) -> dict:
    files = sorted(glob.glob(os.path.join(run_dir, "c*.log")), key=offset_of)
    correct: list[str] = []
    wrong: list[str] = []
    err: list[str] = []
    unparsed: list[str] = []
    by_type: dict[str, list[int]] = defaultdict(lambda: [0, 0])
    incomplete: list[int] = []
    for path in files:
        off = offset_of(path)
        with open(path, errors="replace") as fh:
            text = fh.read()
        if not re.search(r"^PM_RUN_DONE ", text, re.M):
            incomplete.append(off)
        for qid, v in verdicts_in(text).items():
            verdict = classify(v)
            if verdict is None:
                err.append(qid)
                continue
            qtype = v.get("question_type", "unknown")
            by_type[qtype][1] += 1
            if verdict:
                correct.append(qid)
                by_type[qtype][0] += 1
            else:
                wrong.append(qid)
                if v.get("predicted") is None:
                    unparsed.append(qid)
    return {
        "artifacts": len(files),
        "incomplete_contexts": sorted(incomplete),
        "correct": len(correct),
        "wrong": len(wrong),
        "err": err,
        "unparsed_replies": len(unparsed),
        "by_type": {k: {"correct": v[0], "scored": v[1]} for k, v in sorted(by_type.items())},
    }


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--dir", required=True, help="arm artifact directory")
    ap.add_argument("--expected", type=int, required=True, help="shared-context count")
    ap.add_argument("--json", action="store_true", help="machine-readable output")
    args = ap.parse_args(argv)

    result = tally(args.dir)
    scored = result["correct"] + result["wrong"]
    final = (
        result["artifacts"] == args.expected
        and not result["err"]
        and not result["incomplete_contexts"]
    )
    result["scored"] = scored
    result["expected_contexts"] = args.expected
    result["final"] = final
    result["accuracy"] = (100.0 * result["correct"] / scored) if scored else None

    if args.json:
        print(json.dumps(result, indent=2, sort_keys=True))
        return 0

    if scored:
        print(
            f"accuracy(scored-only) = {result['accuracy']:.1f}% "
            f"({result['correct']}/{scored})  unparseable-replies={result['unparsed_replies']}"
        )
        for qtype, row in result["by_type"].items():
            pct = 100.0 * row["correct"] / row["scored"] if row["scored"] else 0.0
            print(f"  {qtype:<48} {pct:>5.1f}% ({row['correct']}/{row['scored']})")
    print(f"ERR({len(result['err'])}): {result['err'][:20]}")
    if final:
        print(
            f"RUN FINAL: accuracy = {result['accuracy']:.1f}% "
            f"({result['correct']}/{scored}) over {args.expected} contexts"
        )
        print(
            f"  Tencent published: {TENCENT_WITH_MEMORY:.0f}% with memory / "
            f"{TENCENT_WITHOUT_MEMORY:.0f}% without. "
            f"Claim the split and the reader model alongside this number."
        )
    else:
        print(
            f"RUN NOT FINAL: contexts={result['artifacts']}/{args.expected}, "
            f"ERR={len(result['err'])}, "
            f"incomplete={result['incomplete_contexts'][:20]} "
            f"(final only at full coverage + ERR=0)"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
