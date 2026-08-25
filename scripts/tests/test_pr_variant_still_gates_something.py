#!/usr/bin/env python3
"""F31 — the reduced PR variant of `conformance-bindings` must still gate.

`conformance-bindings.yml` is the only workflow that starts a Moon and runs the
SDK binding suites. It ran post-merge only, so F30 shipped through a green PR
and turned main red for four runs. The F31 ruling puts a REDUCED variant on the
PR path: Moon + the stub embedder + the Rust per-driver parity step, with both
release SDK builds and both binding suites left to main-push and nightly.

A reduction is one `if:` away from reducing to nothing, and a job whose every
step was skipped reports the same green tick as a job that ran and passed. So
this guard asserts three things that together mean "the PR run still proves
something":

  1. `pull_request` is actually a trigger.
  2. The Rust parity step is NOT gated off on the PR path.
  3. At least one expensive step IS gated off — otherwise the "reduced" variant
     is the full one and the diet decision was silently reversed.

Stdlib only, and it parses the step blocks structurally rather than grepping
for a phrase: the last three guards in this directory that keyed on wording
were each blind to the next family that came along.
"""

import unittest
from pathlib import Path

WORKFLOW = (
    Path(__file__).resolve().parents[2] / ".github" / "workflows" / "conformance-bindings.yml"
)

RUST_PARITY_STEP = "Rust - per-driver backend parity"
# Steps whose whole point is that they are expensive. At least one must be off
# the PR path or nothing was reduced.
EXPENSIVE_STEPS = (
    "Build + install lunaris-py WITH bindings-it + embed-remote",
    "npm install + napi build lunaris-ts WITH bindings-it + embed-remote",
    "Python - full binding suite (live Moon)",
    "TypeScript - full binding suite (live Moon)",
)


def steps_of_job(text: str, job: str) -> dict[str, str]:
    """Map step name -> the step block's text, for one job's `steps:` list.

    Indentation-driven rather than phrase-driven: a job is a 2-space key under
    `jobs:`, its steps are 6-space `- ` items. Returns only named steps, which
    is every step this guard reasons about.
    """
    lines = text.splitlines()
    # Find the job key, then its `steps:` key, then consume until the
    # indentation returns to the job level.
    try:
        job_at = next(i for i, ln in enumerate(lines) if ln.startswith(f"  {job}:"))
    except StopIteration:
        raise AssertionError(f"job {job!r} not found in {WORKFLOW}") from None
    steps_at = next(
        i for i in range(job_at + 1, len(lines)) if lines[i].startswith("    steps:")
    )

    blocks: dict[str, str] = {}
    current_name: str | None = None
    buf: list[str] = []
    for ln in lines[steps_at + 1 :]:
        # A non-blank line at 2-space indent ends this job.
        if ln.strip() and not ln.startswith("      "):
            break
        if ln.startswith("      - "):
            if current_name is not None:
                blocks[current_name] = "\n".join(buf)
            buf = [ln]
            current_name = None
            if "- name:" in ln:
                current_name = ln.split("name:", 1)[1].strip()
        else:
            buf.append(ln)
            if current_name is None and ln.strip().startswith("name:"):
                current_name = ln.split("name:", 1)[1].strip()
    if current_name is not None:
        blocks[current_name] = "\n".join(buf)
    return blocks


def is_skipped_on_pr(block: str) -> bool:
    """Does this step's `if:` exclude the pull_request event?"""
    for ln in block.splitlines():
        stripped = ln.strip()
        if stripped.startswith("if:") and "pull_request" in stripped:
            return True
    return False


class PrVariantStillGatesSomething(unittest.TestCase):
    def setUp(self) -> None:
        self.text = WORKFLOW.read_text()
        self.steps = steps_of_job(self.text, "per-driver-parity")

    def test_the_parser_finds_the_steps_it_reasons_about(self):
        """Guard the guard.

        Every assertion below is of the form "step X is / is not gated". If the
        parser silently returned an empty map — a renamed job, a reindent — all
        of them would pass by finding nothing. So pin the parser first.
        """
        self.assertGreaterEqual(
            len(self.steps), 10, f"parser found only {len(self.steps)} named steps"
        )
        self.assertIn(RUST_PARITY_STEP, self.steps)
        for name in EXPENSIVE_STEPS:
            self.assertIn(name, self.steps, f"{name!r} vanished — update this guard")

    def test_the_parser_can_actually_see_a_pr_gate(self):
        """And prove `is_skipped_on_pr` reports True for something, ever.

        A predicate that always returns False would make the "expensive steps
        are gated" test fail loudly, but would make the "Rust step is not
        gated" test pass vacuously — so it needs its own positive case.
        """
        gated = "      - name: x\n        if: github.event_name != 'pull_request'\n        run: y"
        ungated = "      - name: x\n        run: y"
        self.assertTrue(is_skipped_on_pr(gated))
        self.assertFalse(is_skipped_on_pr(ungated))

    def test_pull_request_is_a_trigger(self):
        head = self.text.split("\njobs:", 1)[0]
        self.assertIn(
            "\n  pull_request:",
            head,
            "conformance-bindings no longer runs on pull_request — the only "
            "Moon-backed binding gate is post-merge again, which is the F31 "
            "defect",
        )

    def test_the_rust_parity_step_still_runs_on_a_pr(self):
        block = self.steps[RUST_PARITY_STEP]
        self.assertFalse(
            is_skipped_on_pr(block),
            "the Rust per-driver parity step is skipped on pull_request, so the "
            "PR run of this job proves nothing while still reporting a green "
            "check. It is the step that exercises lunaris-recipes / "
            "lunaris-retrieve / lunaris-storage-moon, where F26 and F30 lived.",
        )

    def test_the_job_itself_is_not_skipped_on_a_pr(self):
        # A job-level `if:` would skip every step at once, which reports as a
        # green (skipped) check exactly like a passing one.
        body = self.text.split("  per-driver-parity:", 1)[1].split("    steps:", 1)[0]
        self.assertNotIn(
            "pull_request",
            body,
            "per-driver-parity is gated off at JOB level on pull_request — that "
            "skips the Rust step too, which is the whole point of the variant",
        )

    def test_the_expensive_steps_are_actually_off_the_pr_path(self):
        still_on = [n for n in EXPENSIVE_STEPS if not is_skipped_on_pr(self.steps[n])]
        self.assertEqual(
            still_on,
            [],
            f"these expensive steps still run on PRs: {still_on}. The F31 ruling "
            "is a REDUCED variant; running the release SDK builds and the full "
            "binding suites on every PR reverses the 2026-08-18 CI diet without "
            "saying so.",
        )

    def test_offline_smoke_is_off_the_pr_path(self):
        body = self.text.split("  offline-smoke:", 1)[1].split("    steps:", 1)[0]
        self.assertIn(
            "pull_request",
            body,
            "offline-smoke is every SDK build over again with no backend; it is "
            "the other half of the cost the ruling keeps off PRs",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
