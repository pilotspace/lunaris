"""A label-gated workflow must actually fire when the label is applied.

`on: pull_request:` with no explicit `types:` defaults to
`[opened, synchronize, reopened]`. A workflow whose job `if:` reads
`github.event.pull_request.labels.*.name` therefore never runs in response to
someone adding that label -- the job reports `skipping` until an unrelated push
happens to re-trigger it, at which point it looks like the label worked.

That is what `perf-gates.yml` did. Its `perf-bench` opt-in shipped with no
`types:`, and the `perf-bench` label did not exist in the repository at all
until 2026-08-25, so the path had never been taken by anyone. The failure mode
is the worst kind: `skipping` is exactly what a correctly-gated job prints when
it is correctly declining to run.

This asks one question of every workflow: if a job is gated on a PR label, does
the `pull_request` trigger list `labeled`?

Stdlib only -- CI runs every `scripts/tests/test_*.py` with the system Python
and installs nothing, so an import of PyYAML would fail the step rather than
check anything. The parser below is indentation-driven, same approach as
`test_pr_variant_still_gates_something.py`.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

WORKFLOWS = Path(__file__).resolve().parents[2] / ".github" / "workflows"

LABEL_EXPR = "pull_request.labels"


def _indent(line: str) -> int:
    return len(line) - len(line.lstrip(" "))


def _is_content(line: str) -> bool:
    stripped = line.strip()
    return bool(stripped) and not stripped.startswith("#")


def _block(lines: list[str], start: int) -> list[str]:
    """Lines strictly more indented than `lines[start]`, up to the next sibling."""
    base = _indent(lines[start])
    out = []
    for line in lines[start + 1 :]:
        if not _is_content(line):
            out.append(line)
            continue
        if _indent(line) <= base:
            break
        out.append(line)
    return out


def _find(lines: list[str], pattern: str, indent: int) -> int | None:
    rx = re.compile(pattern)
    for i, line in enumerate(lines):
        if _is_content(line) and _indent(line) == indent and rx.match(line.strip()):
            return i
    return None


def pull_request_types(text: str) -> list[str] | None:
    """The `types:` under the top-level `on: pull_request:`.

    Returns None when `pull_request` exists with no `types:` (the defaulting
    case this test exists for), and raises LookupError when there is no
    `pull_request` trigger at all -- two different problems that must not be
    reported as the same one.
    """
    lines = text.splitlines()
    on_at = _find(lines, r"^'?on'?:", 0)
    if on_at is None:
        raise LookupError("no top-level `on:` block")
    on_block = _block(lines, on_at)
    pr_at = _find(on_block, r"^pull_request:", 2)
    if pr_at is None:
        raise LookupError("no `pull_request` trigger")
    pr_block = _block(on_block, pr_at)
    types_at = _find(pr_block, r"^types:", 4)
    if types_at is None:
        return None
    raw = pr_block[types_at].split(":", 1)[1].strip()
    if raw.startswith("["):
        return [t.strip().strip("'\"") for t in raw.strip("[]").split(",") if t.strip()]
    # Block-sequence form: `types:` then `- opened` lines.
    return [
        ln.strip().lstrip("-").strip().strip("'\"")
        for ln in _block(pr_block, types_at)
        if _is_content(ln) and ln.strip().startswith("-")
    ]


def label_gated_jobs(text: str) -> list[str]:
    """Job names whose `if:` reads the PR label list."""
    lines = text.splitlines()
    jobs_at = _find(lines, r"^jobs:", 0)
    if jobs_at is None:
        return []
    jobs_block = _block(lines, jobs_at)
    out = []
    for i, line in enumerate(jobs_block):
        if not (_is_content(line) and _indent(line) == 2 and line.strip().endswith(":")):
            continue
        name = line.strip().rstrip(":")
        body = _block(jobs_block, i)
        if any(LABEL_EXPR in ln for ln in body):
            out.append(name)
    return out


class LabelOptInCanBeOptedInto(unittest.TestCase):
    def test_every_label_gated_workflow_listens_for_labeled(self) -> None:
        checked = 0
        for wf in sorted(WORKFLOWS.glob("*.yml")):
            text = wf.read_text()
            jobs = label_gated_jobs(text)
            if not jobs:
                continue
            checked += 1
            try:
                types = pull_request_types(text)
            except LookupError as e:
                self.fail(f"{wf.name}: jobs {jobs} gate on a PR label but {e}")
            self.assertIsNotNone(
                types,
                f"{wf.name}: jobs {jobs} gate on a PR label, but `pull_request` has no "
                "explicit `types:`. It defaults to [opened, synchronize, reopened], so "
                "applying the label fires nothing and the job stays `skipping`. Add "
                "`types: [opened, synchronize, reopened, labeled]`.",
            )
            self.assertIn(
                "labeled",
                types,
                f"{wf.name}: jobs {jobs} gate on a PR label but `types:` is {types} -- "
                "adding the label cannot start the run.",
            )
        # A scanner that matched nothing would pass this file vacuously, which is
        # the very shape of defect the test exists to catch.
        self.assertGreater(
            checked, 0, "no label-gated workflow found -- the scanner matched nothing"
        )

    def test_the_parser_finds_the_gate_and_the_types_it_reasons_about(self) -> None:
        """Self-check on a known-gated workflow, so a blind parser cannot pass."""
        text = (WORKFLOWS / "perf-gates.yml").read_text()
        self.assertTrue(
            label_gated_jobs(text),
            "perf-gates.yml has a label-gated job; an empty result means the job "
            "scanner is blind",
        )
        self.assertIsNotNone(
            pull_request_types(text),
            "perf-gates.yml declares `types:`; None here means the trigger parser "
            "is blind and every assertion above is vacuous",
        )

    def test_the_parser_reports_missing_types_as_none_not_as_absent_trigger(self) -> None:
        """The two failure modes must stay distinguishable."""
        with_pr_no_types = "on:\n  pull_request:\n\njobs:\n  a:\n    runs-on: x\n"
        self.assertIsNone(pull_request_types(with_pr_no_types))
        no_pr_at_all = "on:\n  push:\n    branches: [main]\n\njobs:\n  a:\n    runs-on: x\n"
        with self.assertRaises(LookupError):
            pull_request_types(no_pr_at_all)


if __name__ == "__main__":
    unittest.main()
