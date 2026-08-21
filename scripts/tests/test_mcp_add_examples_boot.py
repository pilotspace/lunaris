#!/usr/bin/env python3
"""W0.3 — every `claude mcp add` example must set LUNARIS_MCP_STORAGE.

Why this exists
---------------
Since 0.7.0 the MCP server REFUSES TO BOOT when `LUNARIS_MCP_STORAGE` is
unset. Through 0.6.x an unset value opened a per-scope SQLite file; that
backend was deleted, and the server now declines to guess a store rather
than silently mis-route someone's memories.

So a bare `claude mcp add ... -- npx -y @pilotspace/lunaris-mcp` is not a
tidier example. It is one that fails the first time a reader runs it.
Eleven such invocations were live simultaneously -- across the README, the
integration guide, the book, and both package READMEs -- while the README
three paragraphs down said in bold that the variable was required.

The two package READMEs matter most and were the last found: they are what
npmjs.com and crates.io render on the package page, which is where most
readers meet this project rather than in the repo.

Two bugs this file exists to not repeat
---------------------------------------
1. **Self-match.** The first draft was an inline `grep` step in `ci.yml`,
   and it flagged its own pattern literal at ci.yml:347. `ci.yml`'s older
   mcp-install gate documents the same trap and assembles its needle from
   two halves; this one keeps the pattern out of any scanned file by living
   in `scripts/`, and still assembles it so a future move cannot resurrect
   the problem.

2. **Window bleed.** The first draft scanned a fixed 4 lines after each hit
   for the env flag. In `docs/book/src/mcp/claude-code.md` three
   invocations sit back to back, so a mutated bare command matched the
   NEXT example's `LUNARIS_MCP_STORAGE` and the gate stayed green -- it
   failed its own mutation check. A command is now read to its exact end:
   continuation lines while the previous line ends in a backslash, and not
   one line further.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_mcp_add_examples_boot.py
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
REQUIRED = "LUNARIS_MCP" + "_STORAGE"

# `--transport` is what separates a real invocation from prose that merely
# names the command ("...then re-run `claude mcp add`").
INVOCATION = re.compile(r"claude mcp add\b.*--transport")

SKIP_DIRS = {
    ".git", "target", "vendor", "node_modules", "tmp",
    "decisions",          # an ADR records what was true when written
    ".add", ".planning",  # not ours to edit
    "milestones",         # shipped history
}
SKIP_FILES = {"CHANGELOG-archive.md"}


def _candidate_files() -> list[Path]:
    out = []
    for path in REPO_ROOT.rglob("*.md"):
        if SKIP_DIRS & set(path.relative_to(REPO_ROOT).parts):
            continue
        if path.name in SKIP_FILES:
            continue
        out.append(path)
    return sorted(out)


def _command_span(lines: list[str], start: int) -> str:
    """The full command beginning at `start`, following continuations only.

    Stops the moment a line does not end in a backslash. See "Window bleed"
    above: a fixed lookahead reaches the next example and reads its flag.
    """
    span = [lines[start]]
    i = start
    while i < len(lines) and lines[i].rstrip().endswith("\\"):
        i += 1
        if i < len(lines):
            span.append(lines[i])
    return "\n".join(span)


def _bootless_invocations() -> list[str]:
    offenders = []
    for path in _candidate_files():
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        for n, line in enumerate(lines):
            if not INVOCATION.search(line):
                continue
            if REQUIRED not in _command_span(lines, n):
                offenders.append(f"{path.relative_to(REPO_ROOT)}:{n + 1}")
    return offenders


class McpAddExamplesBoot(unittest.TestCase):
    def test_every_invocation_sets_the_required_storage(self) -> None:
        offenders = _bootless_invocations()
        self.assertEqual(
            [],
            offenders,
            "these `claude mcp add` examples omit "
            + REQUIRED
            + ", and the server refuses to boot without it — each one fails "
            "the first time a reader runs it:\n  "
            + "\n  ".join(offenders),
        )

    def test_the_scanner_actually_finds_invocations(self) -> None:
        """A scanner that matches nothing would pass this file forever."""
        seen = sum(
            1
            for p in _candidate_files()
            for ln in p.read_text(encoding="utf-8", errors="replace").splitlines()
            if INVOCATION.search(ln)
        )
        self.assertGreaterEqual(
            seen,
            8,
            f"only {seen} `claude mcp add` invocations found; the docs carried "
            "11 when this gate was written. Either they moved or the pattern "
            "broke — a gate that matches nothing is indistinguishable from one "
            "that passes.",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
