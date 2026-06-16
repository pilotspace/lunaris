#!/usr/bin/env python3
"""mem0-docs-reconcile — doc-claim validator (the ADD "red test").

Pure grep over the two shipped migration docs. RED before the BUILD edits
(stale "opt-in beta" / "200-500 ms" / unsourced "~300 ms" present); GREEN once
both files are reconciled to the GATED gap-analysis. Also guards that the
already-correct POSITIONING / why-lunaris adapter claims stay untouched.

Run: python3 .add/tasks/mem0-docs-reconcile/tests/validate_mem0_docs.py
Exit 0 = all checks pass; exit 1 = at least one failure (prints each).
"""
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]  # repo root (.add/tasks/<slug>/tests/ -> 4 up)

MIRRORS = [
    ROOT / "docs" / "MIGRATING-FROM-MEM0.md",
    ROOT / "docs" / "book" / "src" / "migrating" / "mem0.md",
]
UNTOUCHED = [
    ROOT / "docs" / "POSITIONING.md",
    ROOT / "docs" / "book" / "src" / "getting-started" / "why-lunaris.md",
]

# FORBIDDEN: stale claims that must be ABSENT from both mirror docs after reconcile.
FORBIDDEN = [
    "opt-in beta",          # Mem0 graph is NOT opt-in beta — OSS v3 removed it
    "200–500 ms",           # unsourced Mem0 latency (en-dash form)
    "200-500 ms",           # unsourced Mem0 latency (hyphen form, defensive)
    "~300 ms",              # unsourced Mem0 prose figure
]
# REQUIRED: reconciled markers that must be PRESENT in both mirror docs.
REQUIRED = [
    "removed graph",        # Mem0 OSS v3 removed graph support
    "Platform-only",        # -> Platform-only (Mem0g)
    "no LLM on the read path",
    "10.3 ms",              # Lunaris real strict-replay p50
    "strict-replay",
    "not CI-gated",         # the mandatory caveat that travels with the number
    "1.44",                 # Mem0 published p95 figure (sourced)
]
# A distinctive reconciled sentence that must appear byte-identically in BOTH
# mirrors (mirror-drift guard).
MIRROR_IDENTICAL_MARKER = (
    "Mem0 OSS v3 removed graph support"
)
# UNTOUCHED guard: the already-correct, test-guarded claims must still be there.
UNTOUCHED_MARKERS = {
    "POSITIONING.md": ("Ecosystem (shipped)", ROOT / "docs" / "POSITIONING.md"),
    "why-lunaris(graph row)": ("Mem0g (Platform-only)", ROOT / "docs" / "book" / "src" / "getting-started" / "why-lunaris.md"),
    "why-lunaris(adapters)": ("Ecosystem (shipped)", ROOT / "docs" / "book" / "src" / "getting-started" / "why-lunaris.md"),
}


def main() -> int:
    failures = []

    for path in MIRRORS:
        if not path.exists():
            failures.append(f"MISSING mirror doc: {path}")
            continue
        text = path.read_text(encoding="utf-8")
        rel = path.relative_to(ROOT)
        for bad in FORBIDDEN:
            if bad in text:
                failures.append(f"[stale_claim_remains/unsourced_number] {rel}: forbidden phrase still present: {bad!r}")
        for need in REQUIRED:
            if need not in text:
                failures.append(f"[stale_claim_remains/unsourced_number] {rel}: required reconciled marker absent: {need!r}")
        if MIRROR_IDENTICAL_MARKER not in text:
            failures.append(f"[mirror_drift] {rel}: reconciled graph sentence missing: {MIRROR_IDENTICAL_MARKER!r}")

    # UNTOUCHED guard — the already-correct claims must survive.
    for label, (marker, path) in UNTOUCHED_MARKERS.items():
        if not path.exists():
            failures.append(f"MISSING untouched-guard doc: {path}")
            continue
        if marker not in path.read_text(encoding="utf-8"):
            failures.append(f"[reopened_correct_claim] {label}: expected-untouched marker gone: {marker!r}")

    if failures:
        print("FAIL — mem0-docs-reconcile validator:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("PASS — both mirror docs reconciled to the gated gap-analysis; correct claims untouched.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
