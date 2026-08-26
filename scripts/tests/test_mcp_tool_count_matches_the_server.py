#!/usr/bin/env python3
"""Docs must state the number of MCP tools the server actually registers.

The count appeared in **nine** places across README, both MCP READMEs, two
integration guides, the book, a competitive analysis and a test's doc comment.
Adding `memory.remember` made all nine wrong at once, and nothing would have
said so: a stale count is a sentence that still parses, still reads
confidently, and is only discovered by a user counting the tools in their
client.

The truth is derived from the `#[tool(name = "memory.…")]` attributes in
`crates/lunaris-mcp/src/main.rs` — the same declarations rmcp builds the
router from — so the docs cannot drift from the server again.

**Scope.** Only claims about how many *tools* there are. A number that happens
to be the tool count in some unrelated sentence is not this file's business,
so the patterns below all require a tool word next to the digits.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SERVER = ROOT / "crates" / "lunaris-mcp" / "src" / "main.rs"

SKIP_DIRS = {"target", "node_modules", ".git", "vendor", ".add", ".planning", "__pycache__"}
# Changelogs and plans record what was true when written.
SKIP_FILES = ("docs/planning", "CHANGELOG.md", "docs/CHANGELOG-archive.md")

_TOOL_DECL = re.compile(r'name\s*=\s*"(memory\.[a-z_]+)"')
# "16 tools", "**16 memory tools**", "all 16 MCP tools", "ships 16 tools"
_CLAIM = re.compile(r"\b(\d{1,3})\s*(?:\*\*)?\s*(?:memory\s+|MCP\s+)?tools?\b", re.I)


def registered_tools() -> set[str]:
    names = set(_TOOL_DECL.findall(SERVER.read_text(encoding="utf-8")))
    assert len(names) >= 10, f"only {len(names)} tool declarations parsed — the scan is broken"
    return names


def docs() -> list[Path]:
    out = []
    for pattern in ("*.md", "*.rs"):
        for p in ROOT.rglob(pattern):
            rel = p.relative_to(ROOT).as_posix()
            if any(part in SKIP_DIRS for part in p.relative_to(ROOT).parts):
                continue
            if any(rel.startswith(s) for s in SKIP_FILES):
                continue
            out.append(p)
    assert len(out) > 100, f"scan found only {len(out)} files — the walk is broken"
    return out


def test_every_stated_tool_count_matches_the_server() -> None:
    want = len(registered_tools())
    offenders: list[str] = []
    checked = 0

    for p in docs():
        rel = p.relative_to(ROOT).as_posix()
        for i, line in enumerate(p.read_text(encoding="utf-8").splitlines(), start=1):
            # Only lines that are talking about the Lunaris MCP tool roster.
            if "tool" not in line.lower():
                continue
            if not any(w in line for w in ("MCP", "memory tools", "tools are registered",
                                           "tools work against", "tools are available",
                                           "enumerates all")):
                continue
            for m in _CLAIM.finditer(line):
                count = int(m.group(1))
                # Guard against matching an unrelated number that happens to
                # sit near the word "tool" — only plausible roster sizes.
                if not 5 <= count <= 99:
                    continue
                checked += 1
                if count != want:
                    offenders.append(f"{rel}:{i}: says {count} tools, the server registers {want}")

    assert checked >= 6, (
        f"only {checked} tool-count claims found — the scan stopped matching, and a scan that "
        "matches nothing reports a clean repo"
    )
    assert not offenders, (
        "these state a tool count the server does not register:\n  "
        + "\n  ".join(offenders)
        + f"\n\n`crates/lunaris-mcp/src/main.rs` registers {want}: "
        + ", ".join(sorted(registered_tools()))
    )




# Pages that enumerate the roster rather than merely counting it. A count that
# matches while the list is short is the failure this second test exists for:
# adding `memory.remember` moved every "eight" to "nine" and left every list at
# eight names, and the count check alone was green for it.
ROSTER_PAGES = (
    "README.md",
    "crates/lunaris-mcp/README.md",
    "docs/integration/claude-code.md",
    "docs/book/src/mcp/index.md",
)

_MENTION = re.compile(r"`(memory\.[a-z_]+)`")


def test_every_roster_page_names_every_registered_tool() -> None:
    want = registered_tools()
    problems: list[str] = []

    for rel in ROSTER_PAGES:
        p = ROOT / rel
        assert p.exists(), f"{rel} is gone — update ROSTER_PAGES rather than dropping the check"
        named = set(_MENTION.findall(p.read_text(encoding="utf-8")))
        # Only tools, not every backticked identifier on the page.
        named = {n for n in named if n.startswith("memory.")}

        missing = want - named
        if missing:
            problems.append(f"{rel} never names: {', '.join(sorted(missing))}")
        unknown = named - want
        if unknown:
            problems.append(f"{rel} names tools the server does not register: {', '.join(sorted(unknown))}")

    assert not problems, (
        "roster pages disagree with the server:\n  "
        + "\n  ".join(problems)
        + "\n\nA page that counts the tools correctly but lists fewer of them reads as complete."
    )


if __name__ == "__main__":
    failed = False
    for check in (
        test_every_stated_tool_count_matches_the_server,
        test_every_roster_page_names_every_registered_tool,
    ):
        try:
            check()
        except AssertionError as e:
            print(f"FAIL: {check.__name__}: {e}", file=sys.stderr)
            failed = True
    if failed:
        raise SystemExit(1)
    print("ok: every stated MCP tool count and roster listing matches the server")
