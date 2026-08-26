#!/usr/bin/env python3
"""The agent-wiring docs must name the store URL the installer actually writes.

`scripts/setup-lunaris-agents.py` is the headline install: it installs Moon,
starts it, writes the MCP server entry and installs the lifecycle hooks. Its
`DEFAULT_MOON_URL` is `moon://127.0.0.1:6381`, and the reason is in a comment
right above it — on the reference box 6380 is an unrelated ai-proxy Redis that
answers RESP `PING` happily, so a 6380 default silently wrote memory into the
wrong service on 2026-07-14.

Every agent-facing doc still said 6380. Two of them said, in a table,
"setup writes `moon://127.0.0.1:6380`", which was simply false. The failure
that combination produces is the worst kind: a user follows the doc, hand-wires
`LUNARIS_MCP_STORAGE=moon://127.0.0.1:6380`, the installer's Moon is on 6381,
and nothing is listening — or worse, something else is, and the memories go
somewhere nobody looks. Neither arm announces itself.

The truth is derived from the installer, not restated here, so the docs cannot
drift from it again.

**Scope, deliberately narrow.** `LUNARIS_MCP_STORAGE` is the MCP server's
variable and nothing else's, so every loopback value of it must match. The SDK
examples use `LUNARIS_STORE_URL` and start their OWN Moon on 6380 in the same
snippet — those are internally consistent and correct, and pointing them at the
installer's store would make an example write into the agent's memory. What is
checked there instead is the CLAIM: a doc that says setup writes a URL must
name the URL setup writes.

**And the port a doc *publishes* must be the port it then dials.** The first
sweep of this rule changed the `LUNARIS_MCP_STORAGE` line in the book's MCP
page and left the `docker run -p 6380:6379` two blocks above it alone, so the
page told the reader to publish 6380 and then connect to 6381. That is the same
failure the rule exists to prevent, reintroduced by the fix for it — checking
one end of a pair is indistinguishable from checking both until the pair
disagrees. So a docker publish that fronts a Moon (container port 6379) inside
a file that wires MCP storage must publish the port that storage URL dials.
Files that do NOT wire MCP — the SDK examples, `operations/backends.md`,
`getting-started/installation.md` — start their own Moon and are left alone.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / "scripts" / "setup-lunaris-agents.py"

SKIP_DIRS = {"target", "node_modules", ".git", "vendor", ".add", ".planning", "__pycache__"}
# The plan file is a historical record; correcting quoted history is not the job.
SKIP_FILES = {"docs/planning", "CHANGELOG.md"}

LOOPBACK = ("127.0.0.1", "localhost")


def installer_default_url() -> str:
    text = INSTALLER.read_text(encoding="utf-8")
    m = re.search(r'^DEFAULT_MOON_URL\s*=\s*["\']([^"\']+)["\']', text, re.M)
    assert m, "no DEFAULT_MOON_URL in scripts/setup-lunaris-agents.py"
    return m.group(1)


def docs() -> list[Path]:
    out = []
    for pattern in ("*.md", "*.json"):
        for p in ROOT.rglob(pattern):
            rel = p.relative_to(ROOT).as_posix()
            if any(part in SKIP_DIRS for part in p.relative_to(ROOT).parts):
                continue
            if any(rel.startswith(s) or rel == s for s in SKIP_FILES):
                continue
            out.append(p)
    assert len(out) > 50, f"doc scan found only {len(out)} files — the walk is broken"
    return out


_MCP_STORAGE = re.compile(r'LUNARIS_MCP_STORAGE["\']?\s*[=:]\s*["\']?(moon://[^"\'\s,]+)')
_DOCKER_PUBLISH = re.compile(r"-p\s+(\d+):6379\b")
_SETUP_WRITES = re.compile(r'setup writes\s*`?(moon://[^`\s]+)`?')


def test_agent_docs_name_the_url_the_installer_writes() -> None:
    want = installer_default_url()
    want_port = want.rsplit(":", 1)[1]

    offenders: list[str] = []
    checked_storage = 0
    checked_claims = 0
    checked_docker = 0

    for p in docs():
        rel = p.relative_to(ROOT).as_posix()
        lines = p.read_text(encoding="utf-8").splitlines()
        # A docker publish is only this rule's business in a file that then
        # dials the published port through LUNARIS_MCP_STORAGE.
        wires_mcp = any(_MCP_STORAGE.search(l) for l in lines)
        for i, line in enumerate(lines, start=1):
            if wires_mcp:
                for m in _DOCKER_PUBLISH.finditer(line):
                    checked_docker += 1
                    if m.group(1) != want_port:
                        offenders.append(
                            f"{rel}:{i}: docker publishes host port {m.group(1)} for Moon, "
                            f"but this page then dials {want} — publish {want_port} instead"
                        )
            for m in _MCP_STORAGE.finditer(line):
                url = m.group(1)
                host_port = url.removeprefix("moon://")
                host = host_port.rsplit(":", 1)[0]
                if host not in LOOPBACK:
                    continue  # an illustrative remote host, not this machine
                checked_storage += 1
                if host_port.rsplit(":", 1)[1] != want_port:
                    offenders.append(f"{rel}:{i}: LUNARIS_MCP_STORAGE={url}, installer uses {want}")
            for m in _SETUP_WRITES.finditer(line):
                checked_claims += 1
                if m.group(1) != want:
                    offenders.append(
                        f'{rel}:{i}: claims setup writes {m.group(1)}, it writes {want}'
                    )

    assert checked_storage >= 5, (
        f"only {checked_storage} loopback LUNARIS_MCP_STORAGE values found — the scan stopped "
        "matching, and a scan that matches nothing reports a clean repo"
    )
    assert checked_claims >= 2, (
        f"only {checked_claims} 'setup writes' claims found — the scan stopped matching"
    )
    assert checked_docker >= 2, (
        f"only {checked_docker} Moon docker-publish lines found in MCP-wiring docs — the "
        "scan stopped matching, and a scan that matches nothing reports a clean repo"
    )
    assert not offenders, (
        "these docs wire an agent to a store the installer does not create:\n  "
        + "\n  ".join(offenders)
        + f"\n\nThe installer's DEFAULT_MOON_URL is {want}. A user who follows the doc "
        "instead points at a port with nothing on it — or with something else on it, "
        "which is how memory went into an ai-proxy Redis on 2026-07-14."
    )


if __name__ == "__main__":
    try:
        test_agent_docs_name_the_url_the_installer_writes()
    except AssertionError as e:
        print(f"FAIL: {e}", file=sys.stderr)
        raise SystemExit(1)
    print("ok: agent docs name the store URL the installer writes")
