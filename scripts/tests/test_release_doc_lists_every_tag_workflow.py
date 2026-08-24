#!/usr/bin/env python3
"""`docs/RELEASE.md` must name every workflow a `v*` tag actually fires.

Why this exists
---------------
Through v0.7.0, RELEASE.md said "Exactly four workflows fire on a `v*` tag:
crates-publish, ts-prebuild, mcp-prebuild, python-prebuild". Five fire. The
fifth is `cli-release`, and it is the one that matters most to get wrong: it
is the only tag workflow with `permissions: contents: write`, and it runs
softprops/action-gh-release. So the tag does not merely publish packages —
it cuts the GitHub Release and uploads the CLI binaries.

It was easy to miss by reading. `cli-release.yml`'s `on:` block opens with
`push: branches: [main]` and a long `paths:` filter, so the eye files it as
a main-only workflow; `tags: ["v*"]` sits at the bottom of the same `push:`
mapping. A releaser who trusted the sentence would tag believing no Release
would be cut, and would not look for one.

A sentence in a doc is not a check. This derives the set from the workflow
files, so the doc cannot drift again: add a tag-triggered workflow and this
fails until RELEASE.md names it.

Stdlib only, on purpose — `ci.yml`'s "python guards" step documents that
contract for everything in this directory, so importing PyYAML here would
break the step for a reason unrelated to the thing being guarded.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"
RELEASE_DOC = ROOT / "docs" / "RELEASE.md"


def _strip_comment(line: str) -> str:
    """Drop a trailing `#` comment, honouring quotes.

    A naive split on '#' would mangle `tags: ["v*"] # note` differently from
    `paths: ["**/#hash"]`. Cheap scanner, but it keeps this honest: grepping a
    file without removing comments is how a guard ends up reporting the prose
    that warns about a bug as the bug.
    """
    out, quote = [], None
    for ch in line:
        if quote:
            out.append(ch)
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
            out.append(ch)
        elif ch == "#":
            break
        else:
            out.append(ch)
    return "".join(out)


def _on_block(text: str) -> list[str]:
    """Return the lines of the top-level `on:` mapping, comments stripped.

    `on` is also a YAML 1.1 boolean, so some repos quote it. Accept all three
    spellings rather than silently returning an empty block — an empty block
    would make every assertion below vacuously true.
    """
    lines = text.splitlines()
    start = None
    for i, raw in enumerate(lines):
        line = _strip_comment(raw)
        if re.match(r"""^(on|["']on["']):\s*$""", line):
            start = i + 1
            break
    if start is None:
        return []
    block = []
    for raw in lines[start:]:
        line = _strip_comment(raw)
        if not line.strip():
            block.append(line)
            continue
        if not line[:1].isspace():  # dedented back to column 0 → block over
            break
        block.append(line)
    return block


V_TAG = re.compile(r"""^\s*-\s*["']?v\*["']?\s*$""")


def fires_on_v_tag(path: Path) -> bool:
    block = _on_block(path.read_text(encoding="utf-8"))
    in_tags = False
    tags_indent = 0
    for line in block:
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip())
        if in_tags:
            if indent > tags_indent and V_TAG.match(line):
                return True
            if indent <= tags_indent:
                in_tags = False
        # `tags-ignore:` must NOT count as `tags:`.
        if re.match(r"^\s*tags:\s*$", line):
            in_tags, tags_indent = True, indent
        elif re.match(r"^\s*tags:\s*\[", line):
            if re.search(r"""["']?v\*["']?""", line):
                return True
    return False


class ReleaseDocNamesEveryTagWorkflow(unittest.TestCase):
    def test_parser_finds_the_known_tag_workflows(self) -> None:
        """Guard the guard: if the parser silently matched nothing, every
        assertion below would pass while checking nothing at all."""
        assert WORKFLOWS.is_dir(), f"{WORKFLOWS} missing — scan would be vacuous"
        found = {p.stem for p in WORKFLOWS.glob("*.yml") if fires_on_v_tag(p)}
        assert "crates-publish" in found, (
            "the parser did not find `tags: v*` in crates-publish.yml, which "
            f"provably has it. Parser is broken. Found: {sorted(found)}"
        )
        assert len(found) >= 4, f"implausibly few tag workflows: {sorted(found)}"

    def test_release_doc_names_every_tag_triggered_workflow(self) -> None:
        actual = {p.stem for p in WORKFLOWS.glob("*.yml") if fires_on_v_tag(p)}
        doc = RELEASE_DOC.read_text(encoding="utf-8")
        missing = sorted(w for w in actual if w not in doc)
        assert not missing, (
            "these workflows fire on a `v*` tag but docs/RELEASE.md never "
            f"names them: {missing}\n\n"
            "A releaser reads RELEASE.md to know what a tag sets in motion. "
            "An unnamed tag workflow is one whose output nobody goes looking "
            "for — cli-release, the omission that prompted this guard, is the "
            "one that cuts the GitHub Release.\n"
            f"Full tag-triggered set: {sorted(actual)}"
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
