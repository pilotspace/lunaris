"""`EmbedderConfig` / `RerankerConfig` binding tests — v0.4+ native surface.

Refreshed after the v0.4 N-03 cutover deleted the `fastembed` / `ollama` /
`from_onnx_*` factories (see docs/migration/0.3-to-0.4-native-default.md).
Scenarios mirror the vitest sibling at
crates/lunaris-ts/__test__/embedder_config.spec.mts one-for-one.

Two tiers:

1. **Offline** (default) — factory surface + cheap error paths. The native
   factories read LOCAL files only (model-dir artifacts) — no network, no HF
   Hub download — so a bogus ``model_dir`` / ``gguf_path`` fails fast and
   deterministically on any machine, including a bare CI runner.

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


def test_embedder_config_native_missing_artifacts() -> None:
    """`native(model_dir=...)` with a bogus dir must raise `LunarisError`.

    Local-file read only — no network. A bogus dir means no
    model.safetensors / tokenizer.json / config.json, so
    `NativeEmbedder::open` fails fast.
    """
    with pytest.raises(lunaris.LunarisError):
        lunaris.EmbedderConfig.native(model_dir="/nope/does-not-exist")


def test_embedder_config_native_quantized_unavailable_or_bad_path() -> None:
    """`native_quantized()` with a bogus GGUF path must raise.

    Two valid wheel builds, both MUST raise here: without `embedder-gguf`
    the stub raises a clear `ValueError` naming the feature; with it, the
    missing GGUF path fails the local read with `LunarisError`. Either way
    no model download is involved.
    """
    # Positional: the feature-off stub names its (ignored) params
    # `_gguf_path` / `_model_dir`, so a `gguf_path=` kwarg only exists on
    # feature-on builds. Positional works on both.
    with pytest.raises((ValueError, lunaris.LunarisError)):
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


def test_reranker_config_native_missing_artifacts() -> None:
    """`native(model_dir=...)` with a bogus dir must raise `LunarisError`."""
    with pytest.raises(lunaris.LunarisError):
        lunaris.RerankerConfig.native(model_dir="/nope/does-not-exist")


def test_reranker_config_native_quantized_unavailable_or_bad_path() -> None:
    """Bogus GGUF path raises whether or not `reranker-gguf` is compiled in."""
    # Positional for the same stub-kwarg-naming reason as the embedder test.
    with pytest.raises((ValueError, lunaris.LunarisError)):
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
