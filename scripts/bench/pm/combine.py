#!/usr/bin/env python3
"""Combine a primary PersonaMem run with a second-reader pass over its failures.

    combine.py --primary DIR --secondary DIR [--expected N]

Reads per-question verdict artifacts (questions/c*/<qid>.json) from both
runs. The combined answer set keeps the primary reader's verdict wherever it
was correct and substitutes the secondary reader's verdict for questions the
secondary re-answered (the primary's failures).

HONESTY LABEL: because gold labels routed which questions went to the second
reader, the combined score is an ORACLE TWO-READER ENSEMBLE — an upper bound
on what a two-reader cascade could achieve, NOT a single-reader measurement.
Publish it only alongside the clean per-reader numbers and say how it was
built. A deployable (non-oracle) cascade needs a gold-free routing rule.

ERR verdicts (transport failures) are excluded from the denominator in both
runs (LME H3 discipline); a question ERR in the secondary keeps its primary
verdict.
"""

import argparse
import glob
import json
import os
import sys
from collections import defaultdict


def load(d):
    out = {}
    for f in glob.glob(os.path.join(d, "questions", "*", "*.json")):
        v = json.load(open(f))
        out[v["question_id"]] = v
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--primary", required=True)
    ap.add_argument("--secondary", required=True)
    ap.add_argument("--expected", type=int, default=0, help="expected scored questions")
    a = ap.parse_args()

    prim = load(a.primary)
    sec = load(a.secondary)
    if not prim:
        sys.exit(f"no artifacts under {a.primary}")

    combined, from_secondary, sec_fixed = {}, 0, 0
    for qid, p in prim.items():
        s = sec.get(qid)
        if s is not None and not s.get("error"):
            combined[qid] = s
            from_secondary += 1
            if s["correct"] and not p["correct"]:
                sec_fixed += 1
        else:
            combined[qid] = p

    scored = [v for v in combined.values() if not v.get("error")]
    errs = len(combined) - len(scored)
    correct = sum(v["correct"] for v in scored)
    by_type = defaultdict(lambda: [0, 0])
    for v in scored:
        t = by_type[v["question_type"]]
        t[1] += 1
        t[0] += v["correct"]

    print(f"primary   : {a.primary}")
    print(f"secondary : {a.secondary} (re-answered {from_secondary}, fixed {sec_fixed})")
    print(f"COMBINED (oracle two-reader ensemble — upper bound, see --help):")
    print(f"  accuracy = {100.0 * correct / len(scored):.1f}% ({correct}/{len(scored)})  ERR={errs}")
    for t in sorted(by_type):
        c, n = by_type[t]
        print(f"  {t:48} {100.0 * c / n:5.1f}% ({c}/{n})")
    if a.expected and len(scored) != a.expected:
        sys.exit(f"EXPECTED {a.expected} scored questions, got {len(scored)}")


if __name__ == "__main__":
    main()
