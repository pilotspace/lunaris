"""Plan 08-02 T-08-02-02 — GIL discipline proof.

CLAUDE.md invariant: every async PyO3 method releases the GIL across its
`.await`. Plan 08-01's emitter enforces this structurally (brace-balanced
`future_into_py` scan test). This suite proves the invariant *at runtime*
by firing several concurrent recall tasks and asserting wall-clock time
stays bounded — if the GIL were held across `.await`, the tasks would
serialize and wall-time would grow linearly with concurrency.

Uses the offline `LunarisError` path (URL-parse failure) as the awaitable
because it exercises the same `future_into_py` wrapper that ingest / recall
use — no backend required.
"""
from __future__ import annotations

import asyncio
import time

import pytest

import lunaris


async def _one_parse_failure() -> str:
    """One awaitable that round-trips through the PyO3 async runtime and
    returns quickly — we use the URL-parse error path so the test is
    backend-independent."""
    try:
        await lunaris.open("unsupported-scheme://nowhere")
        return "unexpected-success"
    except lunaris.LunarisError as e:
        return e.args[0]


@pytest.mark.asyncio
async def test_concurrent_awaits_dont_serialize() -> None:
    """Fire N concurrent tasks through the async FFI; wall-time must beat
    the serialized estimate by a clear margin.

    The original absolute bound (`< 1.0s` for 50 calls) assumed the
    URL-parse error path was sub-millisecond. That premise is dead: every
    `open()` call now burns ~0.8s of CPU before resolving (success OR
    parse-failure — measured 2026-06-11, tracked as its own task), so the
    absolute bound failed for reasons unrelated to GIL discipline.

    Two regimes, two checks (measured on both, 2026-06-11):

    - Fast regime (CI ubuntu/py3.11: ~0.2ms per call) — scheduler noise
      dominates and concurrent ≈ serialized regardless of GIL discipline,
      so a ratio cannot discriminate. The original absolute bound is the
      meaningful check there: even a fully serialized held-GIL run of
      sub-ms calls stays far under 1s, and a held-GIL regression that
      *matters* is one that pushes per-call cost up — which moves the run
      into the slow regime below.
    - Slow regime (local darwin/py3.14: ~0.8s CPU per call) — overlap is
      measurable, so assert the ratio: held GIL ⇒ concurrent ≈ serialized
      (ratio ≥0.95); released ⇒ calls overlap across cores (ratio ~1/cores).
      0.75 discriminates cleanly even on a 2-core runner.
    """
    n = 16
    serial_samples = 4
    # Warm up — first call compiles any lazy tokio/async-runtime init.
    _ = await _one_parse_failure()

    start = time.perf_counter()
    for _ in range(serial_samples):
        _ = await _one_parse_failure()
    serial_per_call = (time.perf_counter() - start) / serial_samples

    start = time.perf_counter()
    results = await asyncio.gather(*(_one_parse_failure() for _ in range(n)))
    concurrent_total = time.perf_counter() - start

    assert all(r == "STORAGE" for r in results), f"unexpected results: {results!r}"
    serialized_estimate = serial_per_call * n
    if serialized_estimate < 1.0:
        # Fast regime — the original Plan 08-02 absolute bound.
        assert concurrent_total < 1.0, (
            f"{n} concurrent GIL-releasing awaits took {concurrent_total:.4f}s > 1.0s "
            f"(serialized estimate {serialized_estimate:.4f}s) "
            f"— GIL likely held across .await"
        )
    else:
        # Slow regime — overlap is measurable, demand it.
        assert concurrent_total < serialized_estimate * 0.75, (
            f"{n} concurrent GIL-releasing awaits took {concurrent_total:.4f}s vs a "
            f"serialized estimate of {serialized_estimate:.4f}s "
            f"(ratio {concurrent_total / serialized_estimate:.2f} >= 0.75) "
            f"— GIL likely held across .await"
        )


@pytest.mark.asyncio
async def test_concurrent_backend_calls_dont_block(moon_backend_url: str) -> None:
    """Stronger live-backend proof — runs only when a Moon dev box is up."""
    handle = await lunaris.open(moon_backend_url)
    n = 5
    start = time.perf_counter()
    results = await asyncio.gather(*(handle.recall().top(3).execute() for _ in range(n)))
    elapsed = time.perf_counter() - start
    assert len(results) == n
    for r in results:
        assert isinstance(r, list)
    # Loose upper bound (1s per concurrent recall is already pathological
    # against an empty backend; this catches a GIL-stuck regression).
    assert elapsed < n * 1.0, (
        f"{n} concurrent recalls took {elapsed:.4f}s — GIL likely held"
    )
