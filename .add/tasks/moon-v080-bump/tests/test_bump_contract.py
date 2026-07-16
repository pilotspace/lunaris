#!/usr/bin/env python3
"""Contract gates for moon-v080-bump (target: Moon main e41aa671 = v0.8.0 + PR #351 dashtable fix).

Red-first battery: run BEFORE the bump -> the pin/docs/dashtable tests fail;
run AFTER build -> all green. Live gates (cargo build/test/clippy, recovery
harness, upgrade-replay, poisoned-checkpoint boot) are executed by the
build/verify phases and recorded as evidence in TASK.md §6 — this file checks
the statically checkable contract facts so the red/green flip is cheap and
deterministic.

Usage: python3 test_bump_contract.py   (exits non-zero on any failure)
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]  # .add/tasks/<slug>/tests -> repo root
TARGET_SHA = "e41aa6716fdaed81087f5ecd9623c13c0ec4ee83"  # moon main = v0.8.0 + #351
FAILURES: list[str] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    mark = "PASS" if ok else "FAIL"
    print(f"  [{mark}] {name}" + (f" — {detail}" if detail else ""))
    if not ok:
        FAILURES.append(name)


def sh(*args: str) -> str:
    return subprocess.run(args, capture_output=True, text=True, cwd=ROOT).stdout.strip()


def test_gitlink_pinned() -> None:
    """Scenario: pin lands on the fix-carrying merge commit."""
    out = sh("git", "ls-tree", "HEAD", "vendor/moon")
    sha = out.split()[2] if len(out.split()) > 2 else "?"
    check("gitlink_pinned_v080_fix", sha == TARGET_SHA, f"gitlink={sha[:7]}")


def test_gitmodules_untouched() -> None:
    """Scenario: pin lands (And-clause) — submodule URL unchanged."""
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
    """G2 sdk-parity: moondb stays 0.2.1 (byte-identical SDK)."""
    lock = (ROOT / "Cargo.lock").read_text()
    m = re.search(r'name = "moondb"\nversion = "([^"]+)"', lock)
    check("moondb_lock_0_2_1", bool(m) and m.group(1) == "0.2.1", f"lock={m.group(1) if m else 'absent'}")


def test_moon_lock_bumped() -> None:
    """G3 precondition: Cargo.lock resolves the moon server crate at 0.8.0."""
    lock = (ROOT / "Cargo.lock").read_text()
    m = re.search(r'name = "moon"\nversion = "([^"]+)"', lock)
    check("moon_lock_0_8_0", bool(m) and m.group(1) == "0.8.0", f"lock={m.group(1) if m else 'absent'}")


def test_dashtable_fix_in_tree() -> None:
    """G8 static half: the pinned tree carries the split-retry loop, not the bare unreachable."""
    src = (ROOT / "vendor/moon/src/storage/dashtable/mod.rs").read_text()
    has_loop = "let (final_seg_idx, slot, inserted) = loop {" in src
    # The unreachable! must be GONE from insert_or_update (the fix removed it entirely).
    still_unreachable = 'unreachable!("double NeedsSplit after split_segment")' in src
    check("dashtable_split_retry_loop", has_loop and not still_unreachable,
          f"loop={has_loop} unreachable_present={still_unreachable}")


def test_harness_upgrade_replay_mode() -> None:
    """G7 harness intact: --upgrade-replay leg + space-form MQ probes survive."""
    harness = (ROOT / "scripts" / "test-recovery.py").read_text()
    check("harness_upgrade_replay_mode", "--upgrade-replay" in harness or "upgrade_replay" in harness)
    check("harness_mq_probe", '"MQ", "PUSH"' in harness, "MQ plane probed (space-form wire)")


def test_durability_docs_v08() -> None:
    """G9: durability docs name the v0.8 One-Storage-Kernel story."""
    for rel in ("docs/durability.md", "docs/book/src/operations/durability.md"):
        doc = (ROOT / rel).read_text()
        ok = "0.8.0" in doc and ("kill-9" in doc.lower() or "kill‑9" in doc) and "#353" in doc
        check(f"durability_doc_v08_{Path(rel).parts[-2]}", ok, rel)


def test_embedded_moon_not_default() -> None:
    """G5 guard: embedded-moon stays out of lunaris-mcp default features."""
    toml = (ROOT / "crates/lunaris-mcp/Cargo.toml").read_text()
    m = re.search(r"^default\s*=\s*\[([^\]]*)\]", toml, re.M)
    # No `default = [...]` line at all == empty default set == guard satisfied.
    ok = (m is None) or ("embedded-moon" not in m.group(1))
    check("embedded_moon_not_default", ok,
          f"default={m.group(1).strip() if m else 'absent (empty set)'}")


def main() -> int:
    print("moon-v080-bump contract gates (target e41aa671 = v0.8.0 + #351):")
    for fn in (
        test_gitlink_pinned,
        test_gitmodules_untouched,
        test_pin_reachable_from_remote,
        test_moondb_lock_unchanged,
        test_moon_lock_bumped,
        test_dashtable_fix_in_tree,
        test_harness_upgrade_replay_mode,
        test_durability_docs_v08,
        test_embedded_moon_not_default,
    ):
        fn()
    print(f"\n{len(FAILURES)} failure(s)" + (f": {', '.join(FAILURES)}" if FAILURES else " — ALL GREEN"))
    return 1 if FAILURES else 0


if __name__ == "__main__":
    sys.exit(main())
