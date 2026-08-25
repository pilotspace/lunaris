"""Emit the Python SDK's embedding matrix for the fixed parity corpus.

Driven by `run_sdk_embedder_parity.rs`. Prints nothing but a marker line; the
matrix goes to the output path so a ~1.5 MB payload never crosses a pipe.

`embedder_config_embed_batch` is a `bindings-it` helper, and it lives on the
**native submodule** `lunaris.lunaris` — `python/lunaris/__init__.py` has an
explicit re-export list and does not name it. Probing `lunaris` itself reports
"missing" against a wheel that has it, which the driver would turn into a skip:
a parity test that reports green having compared nothing. Resolve it where it
actually is, and keep exit 3 for a wheel genuinely built without the feature —
or for no wheel at all, which is the case in jobs that build no SDK.
"""
import asyncio
import json
import sys

try:
    from lunaris import lunaris as _native
except ModuleNotFoundError as exc:
    # No wheel installed at all — `feature-build smoke (no backend)` runs
    # `cargo test --features bindings-it` with no SDK build step, so this driver
    # compiles and runs there with nothing to probe. That is the same condition
    # exit 3 already means, but it surfaced as an unhandled import at MODULE
    # scope, which exits 1 before main() can classify it.
    #
    # Narrow on purpose: only a missing `lunaris` package is a skip. A wheel
    # that IS installed but fails to import its own native module is a real
    # defect, and must keep failing loudly.
    if (exc.name or "").split(".")[0] != "lunaris":
        raise
    print(f"WHEEL-NOT-INSTALLED: {exc}", file=sys.stderr)
    sys.exit(3)


async def main() -> int:
    embed_batch = getattr(_native, "embedder_config_embed_batch", None)
    from_env = getattr(_native, "embedder_config_from_env", None)
    if embed_batch is None or from_env is None:
        print("WHEEL-LACKS-BINDINGS-IT", file=sys.stderr)
        return 3
    inputs = json.load(open(sys.argv[1], encoding="utf-8"))
    try:
        cfg = await from_env()
    except Exception as exc:  # noqa: BLE001 - the driver classifies, not us
        print(f"NO-EMBEDDER: {exc}", file=sys.stderr)
        return 4
    matrix = await embed_batch(cfg, inputs)
    if not any(v != 0.0 for row in matrix for v in row):
        # `resolve_default_embedder` never fails when nothing is configured —
        # it warns and hands back NoopEmbedder. Two Noops agree perfectly, so
        # this must be a skip, not a pass.
        print("NO-EMBEDDER: resolved to NoopEmbedder (all-zero vectors)", file=sys.stderr)
        return 4
    with open(sys.argv[2], "w", encoding="utf-8") as fh:
        json.dump(matrix, fh)
    print("PY-MATRIX-OK", len(matrix), len(matrix[0]) if matrix else 0)
    return 0


sys.exit(asyncio.run(main()))
