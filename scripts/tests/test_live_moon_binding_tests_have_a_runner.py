#!/usr/bin/env python3
"""W4.14 — every live-Moon binding test must have a job that satisfies it.

Why this exists
---------------
`test_gated_tests_have_a_runner.py` asks whether a `#![cfg(feature = "X")]`
file has a `cargo test --features X` anywhere. This is the same question one
layer out, for the SDK suites: a test that reads a Moon URL from the
environment and skips when it is absent needs a job that BOTH sets that
variable AND runs that file. Miss either half and the file skips forever,
which the runner reports as a passing suite with a skip line nobody reads.

What it found on the first run (2026-08-22)
-------------------------------------------
Twelve binding test files gate on a live Moon.
`conformance-bindings.yml`'s `per-driver-parity` job — the only job in the
repo that starts one — ran exactly TWO of them:
`crates/lunaris-py/tests/test_backend_parity.py` and
`crates/lunaris-ts/__test__/backend_parity.spec.mts`. The other ten ran only
in the `bindings-it` job, whose own step name says "offline / skip path",
where every one of them skipped.

That included `documentary_parity.spec.mts`, which W2.9 had just rewritten
from a dead dual-backend gate into a satisfiable Moon-only one. The rewrite
was correct and still changed nothing: a satisfiable gate that no job
satisfies is the same green as an unsatisfiable one.

And the repo used TWO spellings for the same variable —
`LUNARIS_MOON_URL` and `LUNARIS_TEST_MOON_URL` — while the job exports only
the first. Five of the ten could not have run even if the job had named
them. Both halves are asserted below, because either alone leaves the bug.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_live_moon_binding_tests_have_a_runner.py
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "conformance-bindings.yml"

TS_TESTS = REPO_ROOT / "crates" / "lunaris-ts" / "__test__"
PY_TESTS = REPO_ROOT / "crates" / "lunaris-py" / "tests"

# The one spelling. Two names let a job satisfy half a suite; see the module
# docstring and the `LUNARIS_CONFORMANCE_STRICT` note in
# `crates/lunaris-conformance/src/skip.rs`.
CANONICAL_ENV = "LUNARIS_MOON_URL"
BANNED_ENV = "LUNARIS_TEST_MOON_URL"

# Files that read a Moon URL but are NOT suites a job should run directly.
NOT_A_SUITE = {"conftest.py"}


def _binding_test_files() -> list[Path]:
    """Every SDK test file that reads a Moon URL from the environment."""
    out = []
    for d, glob in ((TS_TESTS, "*.spec.mts"), (PY_TESTS, "*.py")):
        for p in sorted(d.glob(glob)):
            if p.name in NOT_A_SUITE:
                continue
            body = p.read_text(encoding="utf-8")
            if CANONICAL_ENV in body or BANNED_ENV in body:
                out.append(p)
    return out


def _live_moon_job() -> str:
    """The text of the one job that starts a Moon, from `env:` to the next job."""
    wf = WORKFLOW.read_text(encoding="utf-8")
    start = wf.index("\n  per-driver-parity:")
    # Slice to the next top-level job key, never a fixed line count — a
    # fixed window can land entirely inside a comment block and assert
    # nothing. See [[fixed-window-reads-only-comments]].
    nxt = re.search(r"\n  [a-z0-9][a-z0-9_-]*:\n", wf[start + 1 :])
    end = start + 1 + nxt.start() if nxt else len(wf)
    return wf[start:end]


class OneSpellingForTheMoonUrl(unittest.TestCase):
    def test_no_binding_test_reads_the_second_spelling(self) -> None:
        offenders = [
            str(p.relative_to(REPO_ROOT))
            for p in _binding_test_files()
            if BANNED_ENV in p.read_text(encoding="utf-8")
        ]
        self.assertEqual(
            [],
            offenders,
            f"these files read {BANNED_ENV}, but the only job that starts a "
            f"Moon exports {CANONICAL_ENV}: {offenders}. Two spellings for one "
            f"variable means a job can satisfy half a suite and skip the rest "
            f"while reporting green.",
        )

    def test_the_job_exports_the_canonical_spelling(self) -> None:
        job = _live_moon_job()
        self.assertIn(
            f"{CANONICAL_ENV}: moon://",
            job,
            f"the per-driver-parity job no longer exports {CANONICAL_ENV}. "
            f"Every suite below gates on it; without the export they all skip "
            f"and the job passes having run nothing.",
        )


class EveryLiveMoonSuiteIsNamedByTheJob(unittest.TestCase):
    def test_the_job_runs_every_live_moon_binding_suite(self) -> None:
        job = _live_moon_job()
        files = _binding_test_files()

        # Vacuity floor: if the SDK test directories move, this guard would
        # otherwise scan an empty roster and pass forever.
        self.assertGreaterEqual(
            len(files),
            8,
            f"expected at least 8 live-Moon binding suites, found "
            f"{[str(p.relative_to(REPO_ROOT)) for p in files]}. If the test "
            f"directories moved, fix this guard rather than letting it scan "
            f"nothing.",
        )

        # A whole-suite invocation covers every file under it. Two shapes:
        # `pytest <dir>/` names the directory; `npx vitest run --config X`
        # with no file argument runs the config's whole `include` glob.
        #
        # Tokenized rather than pattern-matched: the obvious regex for
        # "`vitest run` followed only by flags" nests two quantifiers
        # (`(?:\s+--[\w-]+(?:[= ]\S+)?)*`) and CodeQL flagged it as
        # exponential-backtracking (py/redos). A split-and-walk has no
        # backtracking at all and reads more plainly besides.
        def _is_whole_suite_vitest(line: str) -> bool:
            parts = line.split()
            if "vitest" not in parts:
                return False
            rest = parts[parts.index("vitest") + 1 :]
            if not rest or rest[0] != "run":
                return False
            rest = rest[1:]
            skip_next = False
            for tok in rest:
                if skip_next:
                    skip_next = False
                    continue
                if not tok.startswith("-"):
                    # A bare token after `run` is a file/glob filter — this
                    # invocation runs a subset, not the whole suite.
                    return False
                if "=" not in tok:
                    # `--config vitest.config.mts` — the value is the next token.
                    skip_next = True
            return True

        whole_vitest = any(_is_whole_suite_vitest(ln) for ln in job.splitlines())

        def covered(p: Path) -> bool:
            if p.name in job:
                return True
            if str(p.parent.relative_to(REPO_ROOT)) in job:
                return True
            return p.suffix == ".mts" and whole_vitest

        missing = [str(p.relative_to(REPO_ROOT)) for p in files if not covered(p)]
        self.assertEqual(
            [],
            missing,
            f"the per-driver-parity job is the ONLY job that starts a Moon, and "
            f"it never runs these suites: {missing}. They run in the "
            f"`bindings-it` job instead, whose own step name says "
            f"'offline / skip path' — so every one of them skips, every time. "
            f"A satisfiable gate that no job satisfies is the same green as an "
            f"unsatisfiable one.",
        )


class PytestCollectsEveryPythonSuite(unittest.TestCase):
    """A python suite without a `test_` prefix is never collected at all.

    `conversational_parity.py` sat beside eleven `test_*.py` siblings for
    three minor versions. pytest walked past it on every run — not skipped,
    not reported, absent.
    """

    def test_every_python_binding_suite_is_collectable(self) -> None:
        offenders = []
        for p in sorted(PY_TESTS.glob("*.py")):
            if p.name in NOT_A_SUITE or p.name.startswith("test_"):
                continue
            body = p.read_text(encoding="utf-8")
            if re.search(r"^def test_|^\s+def test_", body, re.M):
                offenders.append(str(p.relative_to(REPO_ROOT)))
        self.assertEqual(
            [],
            offenders,
            f"these files define `test_` functions but do not match pytest's "
            f"`test_*.py` collection glob, so they are never collected: "
            f"{offenders}. Rename them.",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
