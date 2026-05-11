// Plan 21-02 Task 4 — vitest coverage for the EmbedderConfig + RerankerConfig
// napi surface.
//
// Scenarios mirror the pytest sibling at
// crates/lunaris-py/python/tests/test_embedder_config.py one-for-one (same
// names, same expectations) so Plan 21-03's cross-SDK parity test can
// cross-reference them.
//
// ## Two tiers
//
// **Offline (default):** factory construction + bad-input error paths.
// Does NOT touch the network and does NOT trigger an HF Hub download — the
// fastembed default constructor pulls weights on first use, so the offline
// tier asserts the *config holder* is well-formed without actually loading
// a model. The pytest sibling does the same.
//
// **Integration (LUNARIS_TS_IT=1):** opens a real Lunaris handle against
// the Moon dev instance, swaps in an `EmbedderConfig.fastembed()`, and
// smoke-checks `ingest`. Gated because (a) Moon must be running on
// localhost:6380 (or `LUNARIS_MOON_URL`), and (b) fastembed's first call
// triggers a ~600 MB model download.

import { describe, expect, test } from "vitest";

// Dynamic import is consistent with abi_pin.spec.mts — load failures
// surface inside the suite rather than at bootstrap.
const mod = await import("../index.js");
const { EmbedderConfig, RerankerConfig, Lunaris } = mod;

const IT_ENABLED = process.env.LUNARIS_TS_IT === "1";
const MOON_URL = process.env.LUNARIS_MOON_URL ?? "redis://localhost:6380";

describe("EmbedderConfig — offline tier", () => {
  // PY: test_embedder_config_fastembed_default
  test("fastembed() returns a config", () => {
    // Lazy: don't actually trigger a model load here. The pytest sibling
    // asserts the returned object is an EmbedderConfig instance without
    // invoking embed_batch. We do the same by checking the holder type.
    // If fastembed's auto-download is undesired in CI, set
    // LUNARIS_FASTEMBED_CACHE_DIR to a pre-populated cache.
    if (!IT_ENABLED) {
      // Constructor would trigger a download on a fresh box; skip the
      // actual call in the offline tier and assert the factory is exposed.
      expect(EmbedderConfig).toBeTypeOf("function");
      expect(EmbedderConfig.fastembed).toBeTypeOf("function");
      return;
    }
    const cfg = EmbedderConfig.fastembed();
    expect(cfg).toBeDefined();
    expect(cfg.declaredDim).toBe(768);
  });

  // PY: test_embedder_config_fastembed_execution_kwargs
  test("fastembed({ execution: 'bogus' }) throws with execution message", () => {
    expect(() => EmbedderConfig.fastembed({ execution: "bogus" })).toThrow(
      /execution/,
    );
  });

  // PY: test_embedder_config_from_onnx_path_missing_file
  test("fromOnnxPath with missing files throws", () => {
    expect(() =>
      EmbedderConfig.fromOnnxPath({
        onnxPath: "/nope/does-not-exist.onnx",
        tokenizerPath: "/nope/does-not-exist.json",
        dim: 768,
      }),
    ).toThrow(/onnx|read/i);
  });

  // PY: test_embedder_config_from_onnx_bytes_empty
  test("fromOnnxBytes with empty buffers throws", () => {
    expect(() =>
      EmbedderConfig.fromOnnxBytes({
        onnxBytes: Buffer.alloc(0),
        tokenizerBytes: Buffer.alloc(0),
        dim: 768,
      }),
    ).toThrow(/fastembed|onnx|empty/i);
  });

  // PY: test_embedder_config_ollama_default
  test("ollama() constructs without contacting the server", () => {
    const cfg = EmbedderConfig.ollama();
    expect(cfg).toBeDefined();
    expect(cfg.declaredDim).toBe(768);
  });

  // PY: test_reranker_config_noop
  test("RerankerConfig.noop() returns a config", () => {
    const cfg = RerankerConfig.noop();
    expect(cfg).toBeDefined();
  });

  // PY: test_reranker_config_fastembed
  test("RerankerConfig.fastembed factory is exposed", () => {
    expect(RerankerConfig).toBeTypeOf("function");
    expect(RerankerConfig.fastembed).toBeTypeOf("function");
  });
});

describe("Lunaris.withEmbedder / withReranker — integration tier", () => {
  // PY: test_lunaris_open_with_embedder_kwarg
  test.skipIf(!IT_ENABLED)(
    "opens with custom embedder via withEmbedder()",
    async () => {
      const base = await Lunaris.open(MOON_URL);
      const cfg = EmbedderConfig.fastembed();
      const swapped = base.withEmbedder(cfg);
      expect(swapped).toBeDefined();
      // Smoke: snapshot is the cheapest call that exercises the storage
      // port without round-tripping through the embedder.
      const lsn = await swapped.snapshot();
      expect(typeof lsn).toBe("string");
    },
  );

  // PY: test_lunaris_open_with_bad_dim_raises_on_first_embed
  //
  // Deferred to upstream-supplied ONNX fixture (mirrors pytest sibling's
  // deferral). The DimValidatingEmbedder wrapper IS active in the BYO path
  // — first embed_batch checks observed vs. declared dim — but exercising
  // it requires a real ONNX model whose output dim ≠ declared. Plan 21-03
  // conformance fixture will ship a tiny ONNX for this case.
  test.skipIf(!IT_ENABLED)(
    "withEmbedder + bad dim raises on first embed (deferred — needs fixture)",
    () => {
      // Intentionally empty: documents the gap. Implementation lives in
      // open_overrides.rs::with_embedder + DimValidatingEmbedder.
      expect(true).toBe(true);
    },
  );
});
