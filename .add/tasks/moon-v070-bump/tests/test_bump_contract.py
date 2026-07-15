#!/usr/bin/env python3
"""Contract gates for moon-v070-bump (target: Moon v0.7.1 = 4161cdc).

Red-first battery: run BEFORE the bump -> the pin + harness-extension tests
fail; run AFTER build -> all green. Live gates (cargo build/test/clippy,
recovery harness runs) are executed by the build/verify phases and recorded
as evidence in TASK.md §6 — this file checks the statically checkable
contract facts so the red/green flip is cheap and deterministic.

Usage: python3 test_bump_contract.py   (exits non-zero on any failure)
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]  # .add/tasks/<slug>/tests -> repo root
TARGET_SHA = "4161cdc04235413c2b4ae8f0c6c864a0f54893db"  # v0.7.1^{commit}
FAILURES: list[str] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    mark = "PASS" if ok else "FAIL"
    print(f"  [{mark}] {name}" + (f" — {detail}" if detail else ""))
    if not ok:
        FAILURES.append(name)


def sh(*args: str) -> str:
    return subprocess.run(args, capture_output=True, text=True, cwd=ROOT).stdout.strip()


def test_gitlink_pinned() -> None:
    """Scenario: gitlink pinned to v0.7.1."""
    out = sh("git", "ls-tree", "HEAD", "vendor/moon")
    sha = out.split()[2] if len(out.split()) > 2 else "?"
    check("gitlink_pinned_v071", sha == TARGET_SHA, f"gitlink={sha[:7]}")


def test_gitmodules_untouched() -> None:
    """Scenario: gitlink pinned to v0.7.1 (And-clause)."""
    gm = (ROOT / ".gitmodules").read_text()
    check(
        "gitmodules_url_unchanged",
        "https://github.com/pilotspace/moon.git" in gm,
        "pilotspace/moon url present",
    )


def test_pin_reachable_from_remote() -> None:
    """Reject guard: not_our_ref — pin must be reachable from origin/main."""
    rc = subprocess.run(
        ["git", "-C", "vendor/moon", "merge-base", "--is-ancestor", TARGET_SHA, "origin/main"],
        cwd=ROOT,
    ).returncode
    check("pin_reachable_not_our_ref", rc == 0)


def test_moondb_lock_unchanged() -> None:
    """Scenario: workspace compiles with zero source changes (And-clause)."""
    lock = (ROOT / "Cargo.lock").read_text()
    m = re.search(r'name = "moondb"\nversion = "([^"]+)"', lock)
    check("moondb_lock_0_2_1", bool(m) and m.group(1) == "0.2.1", f"lock={m.group(1) if m else 'absent'}")


def test_harness_probes_mq_and_temporal() -> None:
    """Scenario: MQ + temporal survive kill-9 — harness must gain the probes."""
    # Fixture correction (build-time, semantics preserved): production publish
    # uses the SPACE form `MQ PUSH` via MqClient::push — dotted spellings are
    # forbidden on the wire (queue.rs contract v1). Original guess "MQ.PUSH"
    # could never match a production-faithful probe.
    harness = (ROOT / "scripts" / "test-recovery.py").read_text()
    check("harness_mq_probe", '"MQ", "PUSH"' in harness, "MQ plane probed (space-form wire)")
    check("harness_temporal_probe", "TEMPORAL.SNAPSHOT_AT" in harness, "temporal plane probed")


def test_harness_upgrade_replay_mode() -> None:
    """Scenario: v0.6-era WAL replays intact under v0.7.1 (#69)."""
    harness = (ROOT / "scripts" / "test-recovery.py").read_text()
    check("harness_upgrade_replay_mode", "--upgrade-replay" in harness or "upgrade_replay" in harness)


def test_durability_docs_refreshed() -> None:
    """Scenario: durability docs refreshed — WAL v3 + #69 + 0.7.1 wording."""
    for rel in ("docs/durability.md", "docs/book/src/operations/durability.md"):
        doc = (ROOT / rel).read_text()
        ok = "WAL v3" in doc and "#69" in doc and "0.7.1" in doc
        check(f"durability_doc_{Path(rel).parts[-2]}", ok, rel)


def main() -> int:
    print("moon-v070-bump contract gates (target v0.7.1 / 4161cdc):")
    for fn in (
        test_gitlink_pinned,
        test_gitmodules_untouched,
        test_pin_reachable_from_remote,
        test_moondb_lock_unchanged,
        test_harness_probes_mq_and_temporal,
        test_harness_upgrade_replay_mode,
        test_durability_docs_refreshed,
    ):
        fn()
    print(f"\n{len(FAILURES)} failure(s)" + (f": {', '.join(FAILURES)}" if FAILURES else " — ALL GREEN"))
    return 1 if FAILURES else 0


if __name__ == "__main__":
    sys.exit(main())
