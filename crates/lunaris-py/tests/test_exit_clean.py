"""Exit-time Metal teardown regression (2026-07-17 Python-worker SIGABRT).

A worker that loaded the llama.cpp embedder, finished ALL its work, and
exited normally still died with exit code 134: CPython does not guarantee
finalization of every object at shutdown, so model buffers were alive when
`exit()` ran ggml-metal's C++ static destructor, whose
`GGML_ASSERT([rsets->data count] == 0)` aborts the process
(`ggml-metal-device.m:622`, llama-cpp-sys-2 0.1.151).

The fix: `lunaris/__init__.py` registers the native `shutdown_inference`
with `atexit` (which runs BEFORE those destructors), freeing every engine
deterministically. This test red/greens the whole loop in a real
subprocess: pre-fix wheels exit 134 here, post-fix wheels exit 0.

Skips when the granite GGUF is not staged (Tier-0 / no-inference
environments) or when no Moon is reachable. It opened `memory://` until
0.7.0 deleted the embedded SQLite backend; the URL now comes from the
`moon_backend_url` fixture, because a hardcoded dead scheme made the worker
fail at `open()` — BEFORE it ever reached the embed this test exists to
measure — while still reporting as a plain assertion failure.
"""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

GGUF = Path(
    os.environ.get(
        "LUNARIS_EMBEDDER_GGUF",
        str(
            Path.home()
            / ".lunaris"
            / "models"
            / "granite-embedding-311m-multilingual-r2.Q4_K_M.gguf"
        ),
    )
)

# Mirrors test_open_ingest_recall._build_episode: default open() resolves the
# llamacpp embedder, and one ingest forces a real embed before normal exit.
WORKER_SNIPPET = r"""
import asyncio
import ulid
import lunaris

ep = {
    "id": str(ulid.ULID()),
    "scope": "_dev_",
    "source": "exit-clean-test",
    "content": "the quick brown fox jumps over the lazy dog",
    "t_ref": None,
    "bt": {
        "valid": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
        "sys": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
    },
    "metadata": {},
}

async def main():
    handle = await lunaris.open(MOON_URL)
    await handle.ingest(ep)

asyncio.run(main())
print("MAIN-DONE", flush=True)
"""


@pytest.mark.skipif(not GGUF.exists(), reason="granite GGUF not staged")
def test_worker_exits_cleanly_after_real_embed(moon_backend_url: str) -> None:
    pytest.importorskip("ulid", reason="python-ulid not installed")
    snippet = f"MOON_URL = {moon_backend_url!r}\n" + WORKER_SNIPPET
    proc = subprocess.run(
        [sys.executable, "-c", snippet],
        capture_output=True,
        text=True,
        timeout=600,
    )
    assert "MAIN-DONE" in proc.stdout, (
        f"worker never finished its work: exit={proc.returncode}\n"
        f"stderr tail:\n{proc.stderr[-2000:]}"
    )
    assert proc.returncode == 0, (
        "worker completed all work but the PROCESS EXIT crashed "
        f"(exit={proc.returncode}) — exit-time inference teardown regressed "
        f"(ggml-metal rsets assert?)\nstderr tail:\n{proc.stderr[-2000:]}"
    )
