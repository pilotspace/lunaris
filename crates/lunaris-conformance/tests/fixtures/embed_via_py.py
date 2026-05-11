"""Plan 21-03 Task 1 — Python-side fixture for cross-SDK embedder parity.

Reads `--corpus` (one input per line, UTF-8), builds
`EmbedderConfig.fastembed(cache_dir=...)` from the `lunaris-py` SDK, runs the
corpus through the inner embedder, and writes the resulting `count × dim`
f32 matrix to `--out` using the shared binary layout:

    [u32 count_le][u32 dim_le][count * dim * 4 bytes f32_le row-major]

Usage:
    python embed_via_py.py --corpus <path> --out <path> --cache-dir <path>

Prerequisites:
    cd crates/lunaris-py && maturin develop --release

The fastembed default downloads ~600 MB of ONNX weights into `--cache-dir`
on first run; subsequent runs reuse the cache.

## Embedder probe contract

The public Python SDK surface for `EmbedderConfig` is opaque — `embed_batch`
is invoked transitively via `Lunaris.ingest` / `Lunaris.recall`. For the
parity fixture we need raw vectors out of the wrapped `Arc<dyn Embedder>`.

This fixture looks for an `embedder_config_embed_batch(cfg, inputs)` free
function on the compiled `lunaris.lunaris` cdylib. That probe is a
`#[cfg(feature = "bindings-it")]` helper that the Plan 21-03 follow-up
landed alongside the parity scaffold; release wheels strip it.

If the probe is missing the fixture exits with code 75
(`EX_TEMPFAIL`-style) and a descriptive stderr message. The Rust
orchestrator treats this as a test failure with the actionable hint
"rebuild lunaris-py with --features bindings-it" — keeping the failure
mode informative rather than crashing on `AttributeError`.
"""

from __future__ import annotations

import argparse
import asyncio
import struct
import sys
from pathlib import Path

import lunaris
from lunaris import EmbedderConfig


def _resolve_probe():
    """Return the probe callable, or None if not exposed in this wheel."""
    cdylib = getattr(lunaris, "lunaris", None)
    if cdylib is None:
        return None
    return getattr(cdylib, "embedder_config_embed_batch", None)


async def _embed_via_probe(probe, corpus: list[str], cache_dir: Path) -> list[list[float]]:
    cfg = EmbedderConfig.fastembed(cache_dir=str(cache_dir))
    # `probe` is async (delegates to Embedder::embed_batch which is async).
    return await probe(cfg, corpus)


def _write_matrix(path: Path, matrix: list[list[float]]) -> None:
    count = len(matrix)
    dim = len(matrix[0]) if count else 0
    with path.open("wb") as fh:
        fh.write(struct.pack("<II", count, dim))
        for row in matrix:
            if len(row) != dim:
                raise SystemExit(
                    f"embed_via_py: ragged matrix — expected dim={dim}, got {len(row)}"
                )
            fh.write(struct.pack(f"<{dim}f", *row))


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--corpus", required=True, type=Path)
    p.add_argument("--out", required=True, type=Path)
    p.add_argument("--cache-dir", required=True, type=Path)
    args = p.parse_args(argv)

    corpus = [
        line for line in args.corpus.read_text(encoding="utf-8").splitlines() if line
    ]
    if not corpus:
        print("embed_via_py: corpus is empty", file=sys.stderr)
        return 2

    args.cache_dir.mkdir(parents=True, exist_ok=True)

    probe = _resolve_probe()
    if probe is None:
        print(
            "embed_via_py: `lunaris.lunaris.embedder_config_embed_batch` probe is not exposed in this wheel.\n"
            "  Rebuild with the bindings-it feature:\n"
            "    cd crates/lunaris-py && maturin develop --release --features bindings-it",
            file=sys.stderr,
        )
        return 75  # EX_TEMPFAIL — Rust orchestrator surfaces this as actionable.

    print(
        f"embed_via_py: lunaris={getattr(lunaris, '__version__', '?')} "
        f"corpus={len(corpus)} cache_dir={args.cache_dir}",
        file=sys.stderr,
    )

    matrix = asyncio.run(_embed_via_probe(probe, corpus, args.cache_dir))
    if len(matrix) != len(corpus):
        print(
            f"embed_via_py: row count mismatch — corpus={len(corpus)} matrix={len(matrix)}",
            file=sys.stderr,
        )
        return 3

    _write_matrix(args.out, matrix)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
