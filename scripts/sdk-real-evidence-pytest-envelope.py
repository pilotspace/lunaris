#!/usr/bin/env python3
"""Convert pytest's JUnit XML into per-scenario JSON envelopes.

Usage: sdk-real-evidence-pytest-envelope.py <junit.xml> <out-dir> <moon-url>

Maps each pytest <testcase> to a py-pytest-<classname>-<name>.json file so the
EVIDENCE.md aggregator can cite real-case evidence per test.
"""
from __future__ import annotations

import json
import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path


def _slug(s: str) -> str:
    return re.sub(r"[^a-zA-Z0-9_-]+", "-", s).strip("-")


def main() -> int:
    if len(sys.argv) != 4:
        print("usage: sdk-real-evidence-pytest-envelope.py <junit.xml> <out-dir> <moon-url>")
        return 2
    xml_path, out_dir, moon_url = sys.argv[1:]
    xml = Path(xml_path)
    if not xml.exists():
        print(f"[envelope] junit xml not found: {xml}")
        return 0
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)

    tree = ET.parse(xml)
    total = 0
    failed = 0
    for tc in tree.iter("testcase"):
        total += 1
        classname = tc.get("classname", "")
        name = tc.get("name", "")
        duration = float(tc.get("time", "0") or 0) * 1000.0
        if tc.find("failure") is not None or tc.find("error") is not None:
            status = "FAIL"
            failed += 1
            err_el = tc.find("failure") if tc.find("failure") is not None else tc.find("error")
            details = {"error": (err_el.get("message") or "").splitlines()[0] if err_el is not None else "unknown"}
        elif tc.find("skipped") is not None:
            status = "SKIP"
            sk = tc.find("skipped")
            details = {"reason": (sk.get("message") or "") if sk is not None else ""}
        else:
            status = "PASS"
            details = {"note": "pytest assertion chain passed"}

        env = {
            "runner": "python-pytest",
            "scenario": f"{classname}::{name}".lstrip(":"),
            "backend": moon_url,
            "status": status,
            "duration_ms": round(duration, 3),
            "details": details,
        }
        fname = f"py-pytest-{_slug(classname.split('.')[-1] or 'top')}--{_slug(name)}.json"
        (out / fname).write_text(json.dumps(env, indent=2))

    summary = {
        "runner": "python-pytest",
        "scenario": "summary",
        "backend": moon_url,
        "total": total,
        "failed": failed,
        "passed": total - failed,  # includes SKIP in passed-or-skipped bucket
        "junit_source": str(xml),
    }
    (out / "py-pytest-summary.json").write_text(json.dumps(summary, indent=2))
    print(f"[envelope] wrote {total} pytest envelopes; failed={failed}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
