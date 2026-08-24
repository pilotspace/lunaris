#!/usr/bin/env python3
"""Report a release binary's size, and fail if it exceeds its ceiling.

Why this exists as a separate script from `crates/lunaris-bench/tests/
binary_size_gate.rs`: that test guards `lunaris-server`, which **no workflow
builds in release and which is not a shipped artifact**. The binaries users
actually download are `lunaris-cli` (npx / cargo install) and `lunaris-mcp`
(npx / uvx), both already built in release by `cli-release.yml` and
`mcp-prebuild.yml` — and neither carried any size gate at all.

The release workflows build per target triple into `target/<triple>/release/`,
so a path is passed in rather than derived.

**The ceilings are deliberately generous, and that is not an oversight.**
Binary size is platform-dependent, and the only measurements available when
this landed were macOS arm64 from one machine:

    lunaris-mcp     14.73 MiB
    lunaris-server  11.56 MiB

Setting a tight ceiling from one host on one OS is the same single-sample
error this repo has been burned by twice (see the chaos-test timing, and F40).
So the ceilings start wide enough to be platform-safe and catch only gross
regressions, and the script ALWAYS reports the measured size — including into
the GitHub job summary — so a real cross-platform baseline accumulates from CI
runs. Tighten them from that data, not from a laptop.

Note also that `binary_size_gate.rs` still documents a "~27 MiB baseline" for
lunaris-server against a 35 MiB ceiling. Measured here at 11.56 MiB, so either
that figure is stale or it was taken on a very different platform/config. It
had never run in CI, so nothing would have noticed the drift either way.
"""

import os
import sys

MIB = 1024 * 1024


def main() -> int:
    if len(sys.argv) != 4:
        sys.stderr.write(
            "usage: scripts/check-binary-size.py <binary-path> <ceiling-mib> <label>\n"
        )
        return 2

    path, ceiling_mib, label = sys.argv[1], float(sys.argv[2]), sys.argv[3]

    if not os.path.isfile(path):
        # Loud, not silent: a missing binary means the build step did not
        # produce what this gate was pointed at, which is a wiring bug. It must
        # never read as "size is fine".
        print(f"::error::{label}: no binary at {path} — the build step did not "
              f"produce it, or this gate is pointed at the wrong path")
        return 1

    size = os.path.getsize(path)
    mib = size / MIB
    ceiling = int(ceiling_mib * MIB)
    line = f"{label}: {size} bytes ({mib:.2f} MiB), ceiling {ceiling_mib:g} MiB"
    print(line)

    # Always record the number, pass or fail — the point is to accumulate a
    # cross-platform baseline, not just to gate.
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as fh:
            status = "over" if size > ceiling else "ok"
            fh.write(f"- **{label}** — {mib:.2f} MiB (ceiling {ceiling_mib:g} MiB) — {status}\n")

    if size > ceiling:
        print(f"::error::{label} is {mib:.2f} MiB, over the {ceiling_mib:g} MiB ceiling. "
              f"If the growth is intended, raise the ceiling in the workflow AND say in "
              f"the PR what added the weight.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
