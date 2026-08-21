#!/usr/bin/env python3
"""W4.8 — every writer has a reader, applied to feature-gated test files.

Why this exists
---------------
A test file that opens with `#![cfg(feature = "X")]` compiles to NOTHING
unless someone passes `--features X`. If no workflow ever does, the file is
not a test: it is source code shaped like reassurance. `cargo test
--workspace` walks straight past it and reports green, and the suite's own
name in a directory listing is the only evidence it was ever meant to run.

This is the sixth distinct instance of one failure shape found on
2026-08-21 alone -- the parked `/v1/forget` UAT, ~65 `[skip]` sites that
passed rather than skipped, RAPTOR's parity suite comparing two empty
trees, an HTTP conformance suite no job ever spawned a server for,
`integration.yml` exporting `MOON_URL` where the suite reads
`LUNARIS_MOON_URL`, and a quickstart advertising release binaries no
workflow builds. Every one is the same sentence: a check that reports
nothing is indistinguishable from a check that passes.

What it found on the first run
------------------------------
Four features gated real test files while NO `cargo test` anywhere in
`.github/workflows/` enabled them:

- `embedded-moon`  -- `lunaris-cli/tests/try_end_to_end.rs`, the three tests
  that prove `lunaris try` actually starts a store and returns hits. The
  project's new front door had zero behavioural coverage in CI.
- `cloud-api`      -- `lunaris-llm/tests/openai_compat_fake_server.rs` and
  `lunaris/tests/remote_llm_wired.rs`. CLAUDE.md makes the extractor and
  verifier slots REMOTE-ONLY, so this is the wiring for the primary LLM
  path. The feature appears in CI only under `cargo check`, which compiles
  a test binary and never runs it.
- `budget-it`      -- `lunaris-bench/tests/budget_assertions.rs`, which reads
  Criterion output and asserts the p50/p99 latency budgets. This is the
  implementation the retracted "latency contract enforced in CI" claim
  (W2.4) pointed at. It has never run in CI.
- `sdk-parity-it`  -- `lunaris-conformance/tests/sdk_embedder_parity.rs`, since
  DELETED (W4.10): it drove both SDKs through `EmbedderConfig.fastembed()`,
  an API the v0.6 llama.cpp-only cutover retired, so its subject no longer
  exists in either SDK. The feature it was gated on went with it.

Design notes worth keeping
--------------------------
`cargo build` and `cargo bench` do NOT count as runners. Both compile a
gated file, so both make it *look* covered while never executing an
assertion -- and `--features llamacpp` reaches this repo's workflows
through `cargo bench` and `cargo build` far more often than through
`cargo test`. Counting a compile as a run would have hidden two of the
four findings above.

Known limitation, stated rather than hidden: a command is credited with
covering a crate when it names that crate with `-p`, or sweeps everything
with `--workspace`/`--all`. It does not verify the command lacks a
`--test`/`--exclude` narrowing that skips the specific file. That is a
looser check than ideal and can only produce false NEGATIVES (a missed
violation), never a false positive.

Stdlib-only (unittest); run directly:
  python3 scripts/tests/test_gated_tests_have_a_runner.py
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"
CRATES = REPO_ROOT / "crates"

# File-level gate: `#![cfg(feature = "X")]`. Inner attribute only -- an inner
# `#[cfg(feature = ...)]` on a single fn is a partial skip, not a dead file.
FILE_GATE = re.compile(r'#!\[cfg\(feature\s*=\s*"([^"]+)"\)\]')

# Only these RUN a test. See "Design notes" above: build/bench compile and
# never assert.
RUNNERS = re.compile(r"\bcargo\s+(?:\+\S+\s+)?(?:test|nextest\s+run)\b")

# An orphaned (crate, feature) pair may sit here ONLY with a reason naming the
# tracking task. An entry without a reason is refused by the test below, which
# is what keeps this from decaying into a rubber stamp.
ALLOWLIST: dict[tuple[str, str], str] = {
    ("lunaris-bench", "budget-it"): (
        "W1.7/S4 -- budget_assertions.rs reads Criterion output and asserts the "
        "blueprint p50/p99 budgets, but its row set still names 'Moon + Postgres' "
        "and Postgres was deleted in 0.7.0, so the assertions cannot be trusted "
        "until the rows are re-derived. Wiring it into CI before that re-derivation "
        "would enforce a contract nobody has re-checked. Tracked as W4.9."
    ),
}


def _gated_test_files() -> list[tuple[str, str, Path]]:
    """(crate, feature, path) for every file-level-gated test file."""
    found = []
    for tests_dir in sorted(CRATES.glob("*/tests")):
        crate = tests_dir.parent.name
        for rs in sorted(tests_dir.rglob("*.rs")):
            m = FILE_GATE.search(rs.read_text(encoding="utf-8", errors="replace"))
            if m:
                found.append((crate, m.group(1), rs))
    return found


def _test_commands() -> list[str]:
    """Every cargo-test command in every workflow, continuations joined.

    YAML `run: |` blocks wrap long cargo invocations across lines with a
    trailing backslash, which splits `-p <crate>` from its `--features`. Join
    those before matching or the crate and the feature never appear together.
    """
    commands = []
    for wf in sorted(WORKFLOWS.glob("*.y*ml")):
        text = wf.read_text(encoding="utf-8", errors="replace")
        text = re.sub(r"\\\s*\n\s*", " ", text)
        for line in text.splitlines():
            if RUNNERS.search(line):
                commands.append(" ".join(line.split()))
    return commands


def _features_of(cmd: str) -> set[str]:
    if "--all-features" in cmd:
        return {"*"}
    feats: set[str] = set()
    for chunk in re.findall(r"(?:--features[= ]|(?<!\w)-F[= ])([A-Za-z0-9,_-]+)", cmd):
        feats.update(f for f in chunk.split(",") if f)
    return feats


def _covers_crate(cmd: str, crate: str) -> bool:
    if re.search(r"--workspace\b|--all\b", cmd):
        return f"--exclude {crate}" not in cmd
    return bool(re.search(rf"-p[= ]{re.escape(crate)}(?!\w)", cmd))


class GatedTestFilesHaveARunner(unittest.TestCase):
    def test_every_feature_gated_test_file_is_run_by_some_workflow(self) -> None:
        commands = _test_commands()
        self.assertTrue(
            commands,
            "found no cargo-test invocation in any workflow -- either the "
            "workflows changed shape or this parser broke. Either way the gate "
            "is blind and must not report success.",
        )

        orphans: dict[tuple[str, str], list[str]] = {}
        for crate, feature, path in _gated_test_files():
            covered = any(
                _covers_crate(cmd, crate)
                and ("*" in _features_of(cmd) or feature in _features_of(cmd))
                for cmd in commands
            )
            if not covered and (crate, feature) not in ALLOWLIST:
                key = (crate, feature)
                orphans.setdefault(key, []).append(
                    str(path.relative_to(REPO_ROOT))
                )

        self.assertEqual(
            {},
            orphans,
            "these test files are gated behind a feature NO `cargo test` in "
            "`.github/workflows/` ever enables, so they have never run in CI "
            "and `cargo test --workspace` reports green without them:\n  "
            + "\n  ".join(
                f"{c} [{f}]: {', '.join(files)}" for (c, f), files in sorted(orphans.items())
            )
            + "\n\nFix by adding a job that runs them with the feature enabled. "
            "Adding the feature to a `cargo build` or `cargo bench` does NOT "
            "count -- both compile the file without executing one assertion. "
            "If the suite genuinely cannot run yet, add it to ALLOWLIST with a "
            "reason naming its tracking task.",
        )

    def test_allowlist_entries_carry_a_reason(self) -> None:
        """An allowlist without reasons is just a way to turn the gate off."""
        for key, reason in ALLOWLIST.items():
            self.assertGreater(
                len(reason.strip()),
                60,
                f"allowlist entry {key} needs a real reason naming its tracking "
                f"task, not a placeholder",
            )

    def test_allowlist_has_no_stale_entries(self) -> None:
        """Entries must describe reality, or they outlive the problem."""
        gated = {(c, f) for c, f, _ in _gated_test_files()}
        stale = sorted(set(ALLOWLIST) - gated)
        self.assertEqual(
            [],
            stale,
            f"ALLOWLIST excuses {stale}, which no longer gates any test file. "
            "Delete the entry -- a permanent exemption for a problem that is "
            "gone is how an allowlist stops meaning anything.",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
