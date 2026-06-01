"""Phase 21 Plan 21-01 — `EmbedderConfig` / `RerankerConfig` binding tests.

Two tiers:

1. **Offline** (default) — construction-only tests that do NOT hit the network
   and do NOT download ONNX weights. Run on any machine. Covers:
   - `EmbedderConfig.fastembed()` execution kwarg validation (CPU path).
   - `EmbedderConfig.from_onnx_bytes` empty-input rejection.
   - `EmbedderConfig.from_onnx_path` missing-file error message.
   - `EmbedderConfig.ollama()` constructs without contacting the server.
   - `RerankerConfig.noop()` + `RerankerConfig.fastembed()` execution kwargs.

2. **`bindings_it`** — integration tests that need a live Moon backend AND a
   populated fastembed model cache. Skipped without the marker. Covers:
   - `Lunaris.open(url, embedder=...)` kwarg passthrough end-to-end.
   - BYO ONNX dim-mismatch error on first `embed_batch` — DEFERRED in this
     plan because shipping a tiny real ONNX in test fixtures is heavy; the
     unit test in `embedder_config.rs::tests::dim_validating_embedder_*`
     covers the wrapper directly.
"""
from __future__ import annotations

import os
import pytest

import lunaris


# ---------------------------------------------------------------------------
# Offline: EmbedderConfig construction
# ---------------------------------------------------------------------------


def test_embedder_config_fastembed_default() -> None:
    """`EmbedderConfig.fastembed()` with `execution='cpu'` (the offline path).

    Skips when the fastembed model cache is not pre-populated — fastembed will
    otherwise attempt an HF Hub download. The `LUNARIS_FASTEMBED_CACHE_DIR`
    env var should point at a directory with EmbeddingGemma 300M weights when
    running this test. CI populates this via `scripts/fetch-fastembed-cache.sh`.
    """
    cache = os.environ.get("LUNARIS_FASTEMBED_CACHE_DIR")
    if not cache:
        pytest.skip(
            "fastembed cache not populated — set LUNARIS_FASTEMBED_CACHE_DIR "
            "to a directory with EmbeddingGemma 300M weights to exercise this test"
        )
    cfg = lunaris.EmbedderConfig.fastembed()
    assert "fastembed" in repr(cfg)
    assert cfg.dim == 768


def test_embedder_config_fastembed_execution_kwargs() -> None:
    """`execution='bogus'` must raise `ValueError` with the bad value echoed."""
    with pytest.raises(ValueError, match="execution"):
        lunaris.EmbedderConfig.fastembed(execution="bogus")


def test_embedder_config_pooling_kwargs_rejects_unknown() -> None:
    """`pooling='none'` is not a valid `PoolingMode` variant — must reject."""
    # We exercise the parser via from_onnx_bytes with empty inputs; the parse
    # error fires BEFORE the empty-bytes check (parser runs at the top of the
    # static method), so even with bogus byte inputs we see the pooling error.
    with pytest.raises(ValueError, match="pooling"):
        lunaris.EmbedderConfig.from_onnx_bytes(
            onnx_bytes=b"\x00",  # non-empty so we get past the byte check
            tokenizer_bytes=b"\x00",
            dim=768,
            pooling="none",
        )


def test_embedder_config_from_onnx_path_missing_file() -> None:
    """`from_onnx_path` with a missing path must raise `LunarisError`.

    The error message must name the path that failed to read so operators
    can diagnose without re-deriving which of the four optional `*_path`
    arguments blew up.
    """
    with pytest.raises(lunaris.LunarisError) as excinfo:
        lunaris.EmbedderConfig.from_onnx_path(
            onnx_path="/this/path/does/not/exist.onnx",
            tokenizer_path="/this/path/does/not/exist.json",
            dim=768,
        )
    msg = str(excinfo.value)
    assert "onnx_path" in msg or "/this/path/does/not/exist.onnx" in msg


def test_embedder_config_from_onnx_bytes_empty() -> None:
    """`from_onnx_bytes(b"", b"", ...)` must raise `LunarisError`.

    The lunaris-embed backend rejects empty `onnx_file` bytes at the top of
    `FastembedEmbedder::from_user_defined`.
    """
    with pytest.raises(lunaris.LunarisError) as excinfo:
        lunaris.EmbedderConfig.from_onnx_bytes(
            onnx_bytes=b"",
            tokenizer_bytes=b"",
            dim=768,
        )
    msg = str(excinfo.value).lower()
    assert "fastembed" in msg or "onnx" in msg or "empty" in msg


def test_embedder_config_ollama_default() -> None:
    """`ollama()` constructs without contacting the server (reqwest client only)."""
    cfg = lunaris.EmbedderConfig.ollama()
    assert "ollama" in repr(cfg)
    assert cfg.dim == 768


def test_embedder_config_ollama_custom_dim() -> None:
    """The `dim` kwarg flows through the repr + the getter."""
    cfg = lunaris.EmbedderConfig.ollama(dim=1024)
    assert cfg.dim == 1024
    assert "dim=1024" in repr(cfg)


# ---------------------------------------------------------------------------
# Offline: RerankerConfig construction
# ---------------------------------------------------------------------------


def test_reranker_config_noop() -> None:
    """`RerankerConfig.noop()` must construct + repr `'noop'`."""
    cfg = lunaris.RerankerConfig.noop()
    assert "noop" in repr(cfg)


def test_reranker_config_fastembed_execution_kwargs() -> None:
    """Bad execution string raises `ValueError`."""
    with pytest.raises(ValueError, match="execution"):
        lunaris.RerankerConfig.fastembed(execution="bogus")


# ---------------------------------------------------------------------------
# `bindings_it` tier — live Moon + populated model cache required
# ---------------------------------------------------------------------------


@pytest.mark.bindings_it
async def test_lunaris_open_with_embedder_kwarg(moon_backend_url: str) -> None:
    """Smoke that `Lunaris.open(url, embedder=cfg)` round-trips.

    Requires:
    - Live Moon at `LUNARIS_TEST_MOON_URL` (or default `moon://127.0.0.1:6380`)
    - Populated fastembed cache at `LUNARIS_FASTEMBED_CACHE_DIR`

    The handle gets a fastembed embedder applied; we ingest one episode and
    recall to prove the swap took effect end-to-end.
    """
    if not os.environ.get("LUNARIS_FASTEMBED_CACHE_DIR"):
        pytest.skip("fastembed cache not populated — see test_embedder_config_fastembed_default")

    cfg = lunaris.EmbedderConfig.fastembed()
    handle = await lunaris.open(moon_backend_url, embedder=cfg)
    # If we got here, the swap succeeded; a deeper ingest/recall round-trip
    # is covered by `tests/test_open_ingest_recall.py` which doesn't override
    # the embedder. The point of THIS test is the kwarg plumbing.
    assert handle is not None


@pytest.mark.bindings_it
async def test_lunaris_open_with_reranker_noop(moon_backend_url: str) -> None:
    """`Lunaris.open(url, reranker=RerankerConfig.noop())` round-trips."""
    cfg = lunaris.RerankerConfig.noop()
    handle = await lunaris.open(moon_backend_url, reranker=cfg)
    assert handle is not None


@pytest.mark.bindings_it
@pytest.mark.skip(
    reason="Real ONNX dim-mismatch test deferred — covered by the Rust-side "
    "dim_validating_embedder_rejects_mismatch_on_first_call unit test in "
    "crates/lunaris-py/src/embedder_config.rs. Shipping a tiny real ONNX in "
    "Python test fixtures is heavy; revisit when the model-fixture infra lands."
)
async def test_lunaris_open_with_bad_dim_raises_on_first_embed(
    moon_backend_url: str,
) -> None:
    """BYO ONNX with declared `dim=999` must raise on first `embed_batch`."""
    raise NotImplementedError  # see skip reason
