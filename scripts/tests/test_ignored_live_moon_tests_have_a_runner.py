#!/usr/bin/env python3
"""F29 — an `#[ignore]` whose precondition CI already meets must have a runner.

Why this exists
---------------
This is the third layer of the same question.

* `test_gated_tests_have_a_runner.py` (W4.8) asks it of `#![cfg(feature = "X")]`
  files: does any job pass `--features X`?
* `test_live_moon_binding_tests_have_a_runner.py` (W4.14) asks it of the SDK
  binding suites: does any job BOTH set the Moon URL AND run that file?
* This one asks it of Rust `#[ignore]`d integration tests: if the reason string
  names a precondition `integration.yml` provides, does any step actually
  un-ignore that file?

W1.1 fixed exactly this for ONE crate. `integration.yml` passes
`--include-ignored` to the `lunaris-storage-moon` step, which un-ignores the
five tests in `list_scopes.rs` and `scope_isolation.rs` whose reasons say
"requires live Moon". No other crate's step passes it — and the
`lunaris-memory` step is scoped `--test moon_parity`, so five ignores there
naming `MOON_URL` never run anywhere, in any workflow.

The ignored-test ratchet in `ci.yml` does not catch this and is not meant to:
it enforces that every ignore carries a REASON and that the total COUNT is
pinned. Both were true of these five the whole time. A reason string is a
promise about what would un-ignore the test; nothing checked whether CI was
already keeping it.

Found 2026-08-23 while auditing F28: `coding_session_memory_smoke.rs` reported
"8 passed; 3 ignored" with `MOON_URL` set — the three live coding-session tests
skipping on a precondition the environment had satisfied. Run with a live Moon
they pass in 7s; `consolidator_scope_isolation` in 22s.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_ignored_live_moon_tests_have_a_runner.py
"""

from __future__ import annotations

import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
CRATES = ROOT / "crates"
WORKFLOWS = ROOT / ".github" / "workflows"

# A reason naming any of these is a promise the integration job already keeps:
# it builds a Moon, port-checks it, and exports both URL spellings.
MOON_PRECONDITIONS = ("MOON_URL", "LUNARIS_MOON_URL", "live Moon")

# Preconditions the job genuinely does NOT provide, so parking is correct.
# Each entry is a substring of the reason string.
UNMET_PRECONDITIONS = (
    "GGUF",           # no model artifacts in CI
    "does not hydrate",  # pins a known Moon gap, not an environment need
    "SIGKILL live run",  # 50x2 chaos run; real wall-clock cost, see below
)

IGNORE_RE = re.compile(r'#\[ignore\s*=\s*"([^"]*)"\]')
COMMENT_RE = re.compile(r"^\s*(//|/\*|\*)")


def package_of(crate_dir: pathlib.Path) -> str:
    """`crates/lunaris` is the package `lunaris-memory` — the directory name is
    not the package name, and assuming it is would make this guard silently
    match nothing for the very crate that motivated it."""
    m = re.search(r'^name\s*=\s*"([^"]+)"', (crate_dir / "Cargo.toml").read_text(), re.M)
    assert m, f"no package name in {crate_dir}/Cargo.toml"
    return m.group(1)


def ignore_sites():
    """(package, test-target, reason) for every reasoned `#[ignore]` under
    `crates/*/tests/`. Comment lines are stripped so prose about `#[ignore]`
    does not count — the same rule `ci.yml`'s ratchet applies."""
    out = []
    for crate_dir in sorted(CRATES.iterdir()):
        tests = crate_dir / "tests"
        # `crates/lunaris-mcp-py` has a `tests/` and no Cargo.toml — it is a
        # Python package living under crates/. Not a Rust target, so it has no
        # `cargo test -p` to check against.
        if not tests.is_dir() or not (crate_dir / "Cargo.toml").is_file():
            continue
        pkg = package_of(crate_dir)
        for path in sorted(tests.rglob("*.rs")):
            for line in path.read_text().splitlines():
                if COMMENT_RE.match(line):
                    continue
                m = IGNORE_RE.search(line)
                if m:
                    out.append((pkg, path.stem, m.group(1)))
    return out


def workflow_text() -> str:
    return "\n".join(p.read_text() for p in sorted(WORKFLOWS.glob("*.yml")))


def steps_that_include_ignored() -> set[str]:
    """Packages with at least one step running `--include-ignored`.

    Matched per STEP, not per file: a step is the run-block containing both a
    `cargo test -p <pkg>` and `--include-ignored`. Scoping to the package is
    what makes this meaningful — `--include-ignored` anywhere in the workflow
    would be satisfied by the storage-moon step and would tell us nothing about
    lunaris-memory, which is precisely the gap that was open."""
    text = workflow_text()
    out = set()
    # Split on step boundaries so one step's flag cannot vouch for another's.
    for block in re.split(r"\n      - name:", text):
        if "--include-ignored" not in block:
            continue
        for m in re.finditer(r"cargo test\s+-p\s+([a-z0-9-]+)", block):
            out.add(m.group(1))
    return out


class IgnoredLiveMoonTestsHaveARunner(unittest.TestCase):
    def test_a_moon_gated_ignore_has_a_step_that_un_ignores_it(self):
        runners = steps_that_include_ignored()
        orphans = []
        for pkg, target, reason in ignore_sites():
            if any(u in reason for u in UNMET_PRECONDITIONS):
                continue
            if not any(p in reason for p in MOON_PRECONDITIONS):
                continue
            if pkg not in runners:
                orphans.append(f"{pkg}::{target} — {reason!r}")
        self.assertEqual(
            orphans,
            [],
            "these tests are parked on a precondition integration.yml ALREADY provides "
            "(it builds a Moon and exports MOON_URL + LUNARIS_MOON_URL), but no step runs "
            "their package with `--include-ignored`, so they run in no workflow at all:\n  "
            + "\n  ".join(orphans)
            + "\n\nEither add `--include-ignored` to a step that runs that package, or "
            "rewrite the reason to name what genuinely blocks it. A reason string is a "
            "promise about what un-ignores the test; this asserts CI is not already "
            "keeping it while the test sits parked.",
        )

    def test_no_reason_names_a_backend_that_was_deleted(self):
        """`PG_URL` cannot be satisfied by anything: 0.7.0 removed Postgres.

        A reason offering two ways to un-ignore, one of which no longer exists,
        reads as twice as satisfiable as it is — and it is how three of the five
        above kept looking like they were waiting on the operator's setup."""
        stale = [
            f"{pkg}::{target} — {reason!r}"
            for pkg, target, reason in ignore_sites()
            if "PG_URL" in reason
        ]
        self.assertEqual(
            stale,
            [],
            "these ignore reasons name PG_URL, a backend deleted in 0.7.0:\n  "
            + "\n  ".join(stale),
        )

    def test_the_guard_can_see_the_storage_moon_step(self):
        """Vacuity floor. If the step-scanner matched nothing, both assertions
        above would pass by finding no runners and no orphans worth naming."""
        self.assertIn(
            "lunaris-storage-moon",
            steps_that_include_ignored(),
            "the scanner found NO step running lunaris-storage-moon with "
            "`--include-ignored`, but integration.yml has had one since W1.1. The "
            "scanner is broken, not the workflow — every other assertion here is "
            "worthless until this passes.",
        )

    def test_there_are_ignores_to_check(self):
        """Second vacuity floor: a parse that finds zero sites is green."""
        self.assertGreater(len(ignore_sites()), 5, "found almost no #[ignore] sites; the parser is broken")


if __name__ == "__main__":
    unittest.main()
