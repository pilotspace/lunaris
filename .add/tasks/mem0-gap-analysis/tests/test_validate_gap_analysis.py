#!/usr/bin/env python3
"""Red/green suite for the gap-analysis gate (TASK mem0-gap-analysis, §4).

Two layers:
  1. GATE SELF-TESTS — prove the validator emits each §1 Reject code (must be GREEN once the
     validator exists; they test the gate, not the deliverable).
  2. DELIVERABLE GATE — the real docs/competitive/mem0-gap-analysis.md must pass (RED until §5 build).

Runs without pytest:  python3 test_validate_gap_analysis.py
Exit 0 only when every layer passes (i.e. after the doc is written and valid).
"""
from __future__ import annotations
import tempfile
from pathlib import Path

import validate_gap_analysis as V

REAL_ANCHOR = "crates/lunaris-core/src/circuit_breaker.rs:CircuitBreaker"
SRC = "https://docs.mem0.ai/ (2026-06-14)"
DIMS = ["reliability", "eval", "observability", "correctness-security",
        "memory-update-intelligence", "multi-level-memory", "graph-quality", "sdk-dx"]


def valid_doc() -> str:
    gap = ["| dimension | mem0_capability | lunaris_reality | evidence_anchor | mem0_source | verdict | severity |",
           "|---|---|---|---|---|---|---|"]
    for i, d in enumerate(DIMS):
        verdict = "at-parity" if i % 2 == 0 else "gap-missing"
        gap.append(f"| {d} | cap {d} | reality {d} | {REAL_ANCHOR} | {SRC} | {verdict} | P1 |")
    backlog = ["| proposed_task_slug | dimension | severity | impact | acceptance_evidence | rough_effort | depends-on |",
               "|---|---|---|---|---|---|---|",
               "| io-failsafe-wiring | reliability | P0 | unlocks fail-safe IO | discriminating test green | M | none |"]
    recon = ["| existing_doc | prior_claim | status | note |",
             "|---|---|---|---|",
             "| POSITIONING.md | beats Mem0 on latency | confirmed | bench re-run |"]
    return "\n".join([
        "# Mem0 Gap Analysis", "",
        "## A. Executive summary", "all dims verdicted", "",
        "## B. Methodology", "sources dated", "",
        "## C. Gap table", *gap, "",
        "## D. Accuracy bench", "recall@3 86% (methodology: strict-replay, like-for-like)", "",
        "## E. Ranked backlog", *backlog, "",
        "## F. Reconciliation", *recon, "",
    ])


def _run(doc_text: str) -> list[str]:
    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
        f.write(doc_text)
        p = Path(f.name)
    try:
        return V.validate(p)
    finally:
        p.unlink()


def _has(errs: list[str], code: str) -> bool:
    return any(e.split(":")[0] == code for e in errs)


# ---- gate self-tests (expected GREEN) ----
def gate_self_tests() -> list[tuple[str, bool, str]]:
    out = []

    errs = _run(valid_doc())
    out.append(("valid_doc_passes", errs == [], f"errs={errs}"))

    # incomplete_coverage: drop a dimension
    t = valid_doc().replace("| graph-quality |", "| zzz-unknown |", 1)
    out.append(("incomplete_coverage", _has(_run(t), "incomplete_coverage"), str(_run(t))))

    # unsourced_claim: strip URL+date from one row
    t = valid_doc().replace(f"| {SRC} |", "| (no source) |", 1)
    out.append(("unsourced_claim", _has(_run(t), "unsourced_claim"), str(_run(t))))

    # unwired_claim: at-parity row pointing at a non-existent / test path
    t = valid_doc().replace(REAL_ANCHOR, "crates/lunaris-core/tests/does_not_exist.rs:Foo")
    out.append(("unwired_claim", _has(_run(t), "unwired_claim"), str(_run(t))))

    # apples_to_oranges: number in §D with no methodology marker
    t = valid_doc().replace("recall@3 86% (methodology: strict-replay, like-for-like)", "recall@3 was 86%")
    out.append(("apples_to_oranges", _has(_run(t), "apples_to_oranges"), str(_run(t))))

    # dangling_p0: backlog row missing impact
    t = valid_doc().replace("| unlocks fail-safe IO |", "|  |", 1)
    out.append(("dangling_p0", _has(_run(t), "dangling_p0"), str(_run(t))))

    return out


def deliverable_gate() -> tuple[str, bool, str]:
    errs = V.validate(V.DEFAULT_DOC)
    return ("real_doc_passes", errs == [], f"errs={errs}")


def main() -> int:
    failed = 0
    print("== GATE SELF-TESTS (expect all PASS) ==")
    for name, ok, detail in gate_self_tests():
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}  {'' if ok else detail}")
        failed += 0 if ok else 1

    print("== DELIVERABLE GATE (RED until §5 build writes the doc) ==")
    name, ok, detail = deliverable_gate()
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}  {'' if ok else detail}")
    failed += 0 if ok else 1

    print(f"\n{'GREEN' if failed == 0 else f'RED — {failed} failing'}")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
