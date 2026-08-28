#!/usr/bin/env python3
"""DREAM-01 — `lunaris-consolidate::dream` must never write.

`build_dream_agenda` is the *planner* half of `/dream`: it surfaces candidate
clusters of ripe raw episodes and returns them. The agent (via the
`memory.distill` MCP tool) is what actually writes the distilled episode. If
this module ever gained a write, `/dream` would start mutating the store from
a read path that callers treat as free of side effects, and the agent-owned
curation contract would silently move into the engine.

The module documents that invariant as "`grep -c atomic_write` on this file
MUST be `0`" — but a literal `grep -c` returns 4 on the untouched file,
because the invariant's OWN doc comments contain the string. An absence-check
that matches the prose describing it can never pass, so the rule shipped
unguarded: before this script, nothing in .github/, scripts/, ci/ or xtask/
referenced dream.rs at all.

This counts real call sites instead: comments stripped, test module excluded.
Mirrors the INGEST-04 step in ci.yml, which strips `^\\s*//` for the same
reason.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

TARGET = Path("crates/lunaris-consolidate/src/dream.rs")
# A write is any `.atomic_write(` receiver call. The mock `impl StoragePort`
# in the test module also *defines* `async fn atomic_write`, which is why the
# test boundary has to be honoured rather than just counting the bare name.
WRITE_CALL = re.compile(r"\.atomic_write\s*\(")
TEST_BOUNDARY = re.compile(r"^\s*(#\[cfg\(test\)\]|mod tests\b)")


def production_lines(text: str) -> list[tuple[int, str]]:
    """Lines before the test module, with line comments removed."""
    out: list[tuple[int, str]] = []
    for n, raw in enumerate(text.splitlines(), start=1):
        if TEST_BOUNDARY.match(raw):
            break
        # Strip `//`, `///` and `//!` comments, including trailing ones.
        code = raw.split("//", 1)[0]
        if code.strip():
            out.append((n, code))
    return out


def main() -> int:
    if not TARGET.exists():
        print(f"::error::DREAM-01 guard cannot find {TARGET} — did the module move? "
              f"Update scripts/check-dream-readonly.py rather than deleting the guard.")
        return 1

    text = TARGET.read_text(encoding="utf-8")
    hits = [(n, line.strip()) for n, line in production_lines(text) if WRITE_CALL.search(line)]

    if hits:
        print(f"::error::DREAM-01 violated: {TARGET} is documented READ-ONLY "
              f"(never calls StoragePort::atomic_write) but has "
              f"{len(hits)} production write call site(s):")
        for n, line in hits:
            print(f"  {TARGET}:{n}: {line}")
        print("  The distiller writes via the memory.distill MCP tool, not from "
              "the agenda planner. If the design really changed, update the "
              "module docs and this guard together.")
        return 1

    print(f"DREAM-01 ok: 0 production atomic_write call sites in {TARGET}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
