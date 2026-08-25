"""W0.7 — the Python SDK can SEE a degraded embedder.

## Why this file exists

`Lunaris::open` falls back to `NoopEmbedder` when no GGUF is reachable, and
that fallback is silent by construction. Every vector is zeros, so hybrid
recall collapses to BM25 plus insertion-order tie-breaks while `recall()`
keeps returning successfully with a plausible-looking hit list.
`NoopEmbedder::dim()` deliberately reports a non-zero dimension so existing
index geometry stays valid — which means inspecting the *results* never
reveals it either.

Rust callers always had `lunaris::resolved_embedder_backend()`. Before this
change `grep -rn degraded crates/lunaris-py/src` returned nothing: a
`pip install lunaris` user had no way to ask. `Lunaris.embedder_backend()`
is that way.

Scenarios mirror the vitest sibling at
`crates/lunaris-ts/__test__/embedder_backend_visibility.spec.mts`
one-for-one.
"""
from __future__ import annotations

import pytest

import lunaris

# The tag set is API — see `EmbedderBackend::as_str` in
# crates/lunaris/src/handle.rs. A caller writing `if backend == "noop"` is
# supported, so changing one of these is a breaking change.
KNOWN_TAGS = {"llamacpp", "openai-remote", "ollama-remote", "noop", "unresolved"}

# The tags that mean "real vectors are NOT being produced". `unresolved` is
# deliberately absent: it means `open` never ran in this process, which is
# "unknown", not "degraded".
DEGRADED_TAGS = {"noop"}


def test_the_accessor_is_exposed_on_the_class() -> None:
    """Runs offline — catches the binding being dropped from the surface.

    This is a shape check, not a behaviour check: it fails if a codegen
    regression stops emitting the method, which is exactly the failure mode
    that would silently restore the W0.7 gap.
    """
    assert hasattr(lunaris.Lunaris, "embedder_backend")
    assert callable(lunaris.Lunaris.embedder_backend)


@pytest.mark.asyncio
async def test_open_records_a_real_backend(moon_backend_url: str) -> None:
    """The discriminating test: `open` must RECORD what it resolved.

    Asserting only "the value is a known tag" would pass against an accessor
    wired to a cell nothing ever writes — `unresolved` is a known tag. So the
    assertion that carries the weight is `!= "unresolved"`: after a real
    `open`, the process HAS resolved a backend, and the SDK must be able to
    say which.
    """
    handle = await lunaris.open(moon_backend_url)
    backend = handle.embedder_backend()

    assert isinstance(backend, str)
    assert backend in KNOWN_TAGS, f"unknown backend tag {backend!r}"
    assert backend != "unresolved", (
        "open() completed but the process reports no resolved embedder backend. "
        "The accessor is reading a cell that resolve_embedder never writes — "
        "the SDK is reporting 'unknown' where it should report the truth."
    )


@pytest.mark.asyncio
async def test_a_degraded_backend_is_nameable(moon_backend_url: str) -> None:
    """A caller can branch on degradation without parsing prose.

    On a machine with a staged GGUF this asserts the healthy branch; on a bare
    runner with no model cache it asserts the degraded branch. Either way the
    point is the same and the test is never vacuous: the returned tag lands on
    exactly one side of the degraded/healthy split, so a caller can act on it.
    """
    handle = await lunaris.open(moon_backend_url)
    backend = handle.embedder_backend()

    degraded = backend in DEGRADED_TAGS
    healthy = backend in (KNOWN_TAGS - DEGRADED_TAGS - {"unresolved"})
    assert degraded != healthy, (
        f"{backend!r} is neither clearly degraded nor clearly healthy — a caller "
        "cannot branch on it, which defeats the purpose of exposing it"
    )


@pytest.mark.asyncio
async def test_the_tag_distinguishes_degraded_from_healthy(moon_backend_url: str) -> None:
    """The anti-hardcode test.

    The three tests above ALL pass against an accessor that returns a hardcoded
    ``"llamacpp"``. This one cannot: it runs ``open`` twice in two FRESH child
    interpreters — one with the staged GGUF, one with ``LUNARIS_EMBEDDER_GGUF``
    pointed at a path that does not exist — and requires the two runs to
    disagree.

    Child processes are not an ergonomic choice, they are the only correct one:
    ``resolve_embedder`` writes a process-global ``OnceLock`` on the first
    ``open``, so a second ``open`` in THIS interpreter would replay the first
    answer no matter what the environment says.
    """
    import os
    import subprocess
    import sys

    script = (
        "import asyncio, lunaris\n"
        "async def main():\n"
        f"    h = await lunaris.open({moon_backend_url!r})\n"
        "    print('TAG=' + h.embedder_backend())\n"
        "asyncio.run(main())\n"
    )

    def run(extra_env: dict[str, str]) -> str:
        env = {**os.environ, **extra_env}
        out = subprocess.run(
            [sys.executable, "-c", script],
            env=env,
            capture_output=True,
            text=True,
            timeout=300,
        )
        for line in out.stdout.splitlines():
            if line.startswith("TAG="):
                return line[len("TAG=") :].strip()
        raise AssertionError(
            f"child produced no TAG= line (rc={out.returncode})\n"
            f"stdout tail: {out.stdout[-400:]}\nstderr tail: {out.stderr[-400:]}"
        )

    as_shipped = run({})
    with_no_model = run({"LUNARIS_EMBEDDER_GGUF": "/nonexistent/no-such-model.gguf"})

    assert as_shipped in KNOWN_TAGS
    assert with_no_model == "noop"

    if as_shipped == "noop":
        # No GGUF staged on this machine, so both arms degrade and the two runs
        # legitimately agree. Say so out loud rather than reporting a green that
        # proved nothing.
        pytest.skip(
            "no embedder is reachable in the ambient environment either, so both "
            "arms report 'noop' and this machine cannot show the two apart"
        )

    assert as_shipped != with_no_model, (
        "pointing LUNARIS_EMBEDDER_GGUF at a missing file changed nothing — the "
        "accessor is not reading the resolved backend"
    )
