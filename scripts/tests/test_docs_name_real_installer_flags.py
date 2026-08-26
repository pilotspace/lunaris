#!/usr/bin/env python3
"""Docs must not name a `setup-lunaris-agents.py` flag that does not exist.

Written because the README claimed "`--uninstall` reverses it" while drafting
the headline-install section. There is no `--uninstall`. Nothing would have
caught it: the sentence is plausible, it sits next to true sentences, and no
test executes prose. A reader following it gets `error: unrecognized arguments`
at the exact moment they are trying to undo a half-finished install.

The truth is derived from the installer's own `add_argument` calls, so a
renamed or removed flag fails here instead of in a user's terminal.

**Scope.** Only long flags attached to `scripts/setup-lunaris-agents.py` —
found by scanning for the script name in a fenced command and reading the
flags on that command line. Flags belonging to other tools on other lines are
none of this file's business.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / "scripts" / "setup-lunaris-agents.py"
SCRIPT_NAME = "setup-lunaris-agents.py"

SKIP_DIRS = {"target", "node_modules", ".git", "vendor", ".add", ".planning", "__pycache__"}
# Historical records quote commands as they were; correcting history is not the job.
SKIP_FILES = ("docs/planning", "CHANGELOG.md", "docs/CHANGELOG-archive.md")

_ADD_ARG = re.compile(r'add_argument\(\s*["\'](--[a-z0-9-]+)["\']')
_FLAG = re.compile(r'(?<![\w-])(--[a-z][a-z0-9-]+)')


def real_flags() -> set[str]:
    text = INSTALLER.read_text(encoding="utf-8")
    flags = set(_ADD_ARG.findall(text))
    # BooleanOptionalAction silently also accepts the --no- form.
    for f in list(flags):
        if f"'{f}',\n        action=argparse.BooleanOptionalAction" in text or (
            f'"{f}",\n        action=argparse.BooleanOptionalAction' in text
        ):
            flags.add("--no-" + f.removeprefix("--"))
    flags.add("--help")
    assert len(flags) >= 15, f"only {len(flags)} flags parsed — the add_argument scan is broken"
    return flags


def docs() -> list[Path]:
    out = []
    for p in ROOT.rglob("*.md"):
        rel = p.relative_to(ROOT).as_posix()
        if any(part in SKIP_DIRS for part in p.relative_to(ROOT).parts):
            continue
        if any(rel.startswith(s) for s in SKIP_FILES):
            continue
        out.append(p)
    assert len(out) > 50, f"doc scan found only {len(out)} files — the walk is broken"
    return out


def test_every_documented_installer_flag_exists() -> None:
    known = real_flags()
    offenders: list[str] = []
    checked = 0

    for p in docs():
        rel = p.relative_to(ROOT).as_posix()
        for i, line in enumerate(p.read_text(encoding="utf-8").splitlines(), start=1):
            if SCRIPT_NAME not in line:
                continue
            # Only the flags to the right of the script name are its own.
            tail = line.split(SCRIPT_NAME, 1)[1]
            for flag in _FLAG.findall(tail):
                checked += 1
                if flag not in known:
                    offenders.append(f"{rel}:{i}: {SCRIPT_NAME} {flag} — no such flag")

    assert checked >= 8, (
        f"only {checked} installer flags found in docs — the scan stopped matching, "
        "and a scan that matches nothing reports a clean repo"
    )
    assert not offenders, (
        "these docs name a setup-lunaris-agents.py flag that does not exist:\n  "
        + "\n  ".join(offenders)
        + "\n\nA reader who follows them gets 'unrecognized arguments'. Real flags:\n  "
        + " ".join(sorted(known))
    )


if __name__ == "__main__":
    try:
        test_every_documented_installer_flag_exists()
    except AssertionError as e:
        print(f"FAIL: {e}", file=sys.stderr)
        raise SystemExit(1)
    print("ok: every documented installer flag exists")
