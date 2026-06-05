"""
__main__.py — os.execv shim for `uvx lunaris-mcp` / `python -m lunaris_mcp`.

Resolves the prebuilt native binary from the lunaris_mcp/bin/ directory
(populated at wheel-build time by the mcp-prebuild.yml CI job, or by
manual copy for local development) and execs it, passing all arguments
through. stdio is inherited so the MCP stdio transport works without
wrapping.

Trust boundary: PyPI's own hash verification (PEP 427 + TLS) is the
trust anchor for the wheel distribution path.
"""
from __future__ import annotations

import os
import pathlib
import sys


def main() -> None:  # pragma: no cover
    """Entry point for the `lunaris-mcp` console script and `python -m lunaris_mcp`."""
    pkg_dir = pathlib.Path(__file__).parent
    bin_name = "lunaris-mcp.exe" if sys.platform == "win32" else "lunaris-mcp"
    binary = pkg_dir / "bin" / bin_name

    if not binary.exists():
        print(
            f"[lunaris-mcp] Binary not found at {binary}\n"
            "The wheel may not have been built with the binary included.\n"
            "Fallback: install the Rust toolchain and run: cargo install lunaris-mcp\n"
            "See: https://github.com/pilotspace/lunaris/blob/main/docs/integration/claude-code.md",
            file=sys.stderr,
        )
        sys.exit(1)

    # os.execv replaces the current process — stdio passes through cleanly.
    os.execv(str(binary), sys.argv)


if __name__ == "__main__":
    main()
