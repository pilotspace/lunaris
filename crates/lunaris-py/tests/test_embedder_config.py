"""`EmbedderConfig` / `RerankerConfig` binding tests — v0.6 llama.cpp-only surface.

Refreshed after the llama.cpp-only cutover deleted the candle
`native()` / `native_quantized()` backends (see
docs/migration/0.5-to-0.6-llamacpp-only.md); those factories survive as
loud migration-hint stubs. Scenarios mirror the vitest sibling at
crates/lunaris-ts/__test__/embedder_config.spec.mts one-for-one.

Two tiers:

1. **Offline** (default) — factory surface + cheap error paths. The llamacpp
   factories read LOCAL files only (the GGUF artifact) — no network, no HF
   Hub download — so a bogus ``gguf_path`` fails fast and deterministically
   on any machine, including a bare CI runner.

2. **`bindings_it`** — integration tests that need a live Moon backend.
   The embedder-kwarg plumbing test uses the model-free ``noop()`` config so
   no model cache is required.
"""
from __future__ import annotations

import pytest

import lunaris


# ---------------------------------------------------------------------------
# Offline: EmbedderConfig
# ---------------------------------------------------------------------------


def test_embedder_config_noop_default() -> None:
    """`noop()` constructs with the granite-r2 default dim (768)."""
    cfg = lunaris.EmbedderConfig.noop()
    assert "noop" in repr(cfg)
    assert cfg.dim == 768


def test_embedder_config_noop_custom_dim() -> None:
    """The `dim` kwarg flows through the getter + the repr."""
    cfg = lunaris.EmbedderConfig.noop(dim=512)
    assert cfg.dim == 512
    assert "dim=512" in repr(cfg)


def test_embedder_config_llamacpp_missing_gguf() -> None:
    """`llamacpp(gguf_path=...)` with a bogus path must raise.

    Local-file read only — no network. Two valid wheel builds, both MUST
    raise here: a Tier-0 wheel (no `llamacpp` feature) raises the
    no-inference `ValueError` stub; a full wheel fails the local GGUF read
    with `LunarisError`.
    """
    assert hasattr(lunaris.EmbedderConfig, "llamacpp")
    # Positional: the Tier-0 stub names its (ignored) param `_gguf_path`,
    # so a `gguf_path=` kwarg only exists on feature-on builds.
    with pytest.raises((ValueError, lunaris.LunarisError)):
        lunaris.EmbedderConfig.llamacpp("/nope/does-not-exist.gguf")


def test_embedder_config_retired_native_factories() -> None:
    """`native()` / `native_quantized()` raise the migration hint.

    Pin the MESSAGE (not just "raises") — a Tier-0/full-build artifact
    error would also raise, and this test must discriminate the retirement
    stub from an artifact failure.
    """
    with pytest.raises(ValueError, match="llama.cpp-only cutover"):
        lunaris.EmbedderConfig.native("/nope/does-not-exist")
    with pytest.raises(ValueError, match="llama.cpp-only cutover"):
        lunaris.EmbedderConfig.native_quantized("/nope/does-not-exist.gguf")


def test_embedder_config_deleted_factories_gone() -> None:
    """The v0.3 factories are deleted — assert ABSENCE via hasattr.

    hasattr (not an error-message regex) so this can never false-pass the
    way the old TS `fromOnnxPath` test did, where the "is not a function"
    TypeError happened to match the test's /onnx|read/i throw pattern.
    """
    for name in ("fastembed", "ollama", "from_onnx_path", "from_onnx_bytes"):
        assert not hasattr(lunaris.EmbedderConfig, name), (
            f"EmbedderConfig.{name} was deleted in v0.4 N-03 but is exposed again"
        )


# ---------------------------------------------------------------------------
# Offline: RerankerConfig
# ---------------------------------------------------------------------------


def test_reranker_config_noop() -> None:
    """`RerankerConfig.noop()` must construct + repr `'noop'`."""
    cfg = lunaris.RerankerConfig.noop()
    assert "noop" in repr(cfg)


def test_reranker_config_llamacpp_missing_gguf() -> None:
    """Bogus GGUF path raises on both Tier-0 and full wheel builds."""
    assert hasattr(lunaris.RerankerConfig, "llamacpp")
    with pytest.raises((ValueError, lunaris.LunarisError)):
        lunaris.RerankerConfig.llamacpp("/nope/does-not-exist.gguf")


def test_reranker_config_retired_native_factories() -> None:
    """`native()` / `native_quantized()` raise the migration hint."""
    with pytest.raises(ValueError, match="llama.cpp-only cutover"):
        lunaris.RerankerConfig.native("/nope/does-not-exist")
    with pytest.raises(ValueError, match="llama.cpp-only cutover"):
        lunaris.RerankerConfig.native_quantized("/nope/does-not-exist.gguf")


def test_reranker_config_deleted_factories_gone() -> None:
    """The v0.3 `fastembed` reranker factory is deleted."""
    assert not hasattr(lunaris.RerankerConfig, "fastembed"), (
        "RerankerConfig.fastembed was deleted in v0.4 N-03 but is exposed again"
    )


# ---------------------------------------------------------------------------
# `bindings_it` tier — live Moon required (no model cache needed)
# ---------------------------------------------------------------------------


@pytest.mark.bindings_it
async def test_lunaris_open_with_embedder_kwarg(moon_backend_url: str) -> None:
    """Smoke that `Lunaris.open(url, embedder=cfg)` round-trips.

    Uses the model-free `noop()` config — the point of THIS test is the
    kwarg plumbing, not the embedding math, so it needs Moon but no model
    artifacts.
    """
    cfg = lunaris.EmbedderConfig.noop()
    handle = await lunaris.open(moon_backend_url, embedder=cfg)
    assert handle is not None


@pytest.mark.bindings_it
async def test_lunaris_open_with_reranker_noop(moon_backend_url: str) -> None:
    """`Lunaris.open(url, reranker=RerankerConfig.noop())` round-trips."""
    cfg = lunaris.RerankerConfig.noop()
    handle = await lunaris.open(moon_backend_url, reranker=cfg)
    assert handle is not None


@pytest.mark.bindings_it
@pytest.mark.skip(
    reason="Dim-mismatch test deferred — covered by the Rust-side "
    "dim_validating_embedder_rejects_mismatch_on_first_call unit test in "
    "crates/lunaris-py/src/embedder_config.rs. Exercising it from Python "
    "needs real model artifacts whose output dim != declared; revisit when "
    "the model-fixture infra lands."
)
async def test_lunaris_open_with_bad_dim_raises_on_first_embed(
    moon_backend_url: str,
) -> None:
    """BYO embedder with declared `dim=999` must raise on first `embed_batch`."""
    raise NotImplementedError  # see skip reason
