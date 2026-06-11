// vitest coverage for the EmbedderConfig + RerankerConfig napi surface,
// refreshed to the v0.4+ native API (N-03 cutover deleted the fastembed /
// ollama / fromOnnx* factories — see docs/migration/0.3-to-0.4-native-default.md).
//
// Scenarios mirror the pytest sibling at
// crates/lunaris-py/tests/test_embedder_config.py one-for-one (same names,
// same expectations).
//
// ## Two tiers
//
// **Offline (default):** factory surface + cheap error paths. The native
// factories read LOCAL files only (model dir artifacts) — no network, no HF
// Hub download — so a bogus modelDir/ggufPath fails fast and deterministically
// on any machine, including a bare CI runner.
//
// **Integration (LUNARIS_TS_IT=1):** opens a real Lunaris handle against the
// Moon dev instance and smoke-checks the withEmbedder plumbing with the
// model-free noop() config. Gated because Moon must be running on
// localhost:6380 (or `LUNARIS_MOON_URL`).

import { describe, expect, test } from "vitest";

// Dynamic import is consistent with abi_pin.spec.mts — load failures
// surface inside the suite rather than at bootstrap.
const mod = await import("../index.js");
const { EmbedderConfig, RerankerConfig, Lunaris } = mod;

const IT_ENABLED = process.env.LUNARIS_TS_IT === "1";
const MOON_URL = process.env.LUNARIS_MOON_URL ?? "redis://localhost:6380";

describe("EmbedderConfig — offline tier", () => {
  // PY: test_embedder_config_noop_default
  test("noop() returns a config with granite-r2 default dim", () => {
    const cfg = EmbedderConfig.noop();
    expect(cfg).toBeDefined();
    expect(cfg.declaredDim).toBe(768);
  });

  // PY: test_embedder_config_noop_custom_dim
  test("noop(dim) flows through declaredDim", () => {
    const cfg = EmbedderConfig.noop(512);
    expect(cfg.declaredDim).toBe(512);
  });

  // PY: test_embedder_config_native_missing_artifacts
  test("native() factory is exposed and fails fast on a missing model dir", () => {
    expect(EmbedderConfig.native).toBeTypeOf("function");
    // Local-file read only — no network. A bogus dir means no
    // model.safetensors / tokenizer.json / config.json, so open() throws.
    expect(() =>
      EmbedderConfig.native({ modelDir: "/nope/does-not-exist" }),
    ).toThrow();
  });

  // PY: test_embedder_config_native_quantized_unavailable_or_bad_path
  test("nativeQuantized() throws on a missing GGUF (or without embedder-gguf)", () => {
    expect(EmbedderConfig.nativeQuantized).toBeTypeOf("function");
    // Two valid builds, both MUST throw here: without `embedder-gguf` the
    // stub raises a clear feature error; with it, the missing GGUF path
    // fails the local read. Either way no model download is involved.
    expect(() =>
      EmbedderConfig.nativeQuantized({ ggufPath: "/nope/does-not-exist.gguf" }),
    ).toThrow();
  });

  // PY: test_embedder_config_deleted_factories_gone
  test("v0.3 factories are deleted (no accidental-pass regex matching)", () => {
    // N-03 cutover removed these. Assert ABSENCE via typeof so this can
    // never false-pass the way the old fromOnnxPath test did (its
    // "is not a function" TypeError happened to match /onnx|read/i).
    const e = EmbedderConfig as unknown as Record<string, unknown>;
    expect(typeof e.fastembed).toBe("undefined");
    expect(typeof e.ollama).toBe("undefined");
    expect(typeof e.fromOnnxPath).toBe("undefined");
    expect(typeof e.fromOnnxBytes).toBe("undefined");
  });
});

describe("RerankerConfig — offline tier", () => {
  // PY: test_reranker_config_noop
  test("noop() returns a config", () => {
    const cfg = RerankerConfig.noop();
    expect(cfg).toBeDefined();
  });

  // PY: test_reranker_config_native_missing_artifacts
  test("native() factory is exposed and fails fast on a missing model dir", () => {
    expect(RerankerConfig.native).toBeTypeOf("function");
    expect(() =>
      RerankerConfig.native({ modelDir: "/nope/does-not-exist" }),
    ).toThrow();
  });

  // PY: test_reranker_config_native_quantized_unavailable_or_bad_path
  test("nativeQuantized() throws on a missing GGUF (or without reranker-gguf)", () => {
    expect(RerankerConfig.nativeQuantized).toBeTypeOf("function");
    expect(() =>
      RerankerConfig.nativeQuantized({ ggufPath: "/nope/does-not-exist.gguf" }),
    ).toThrow();
  });

  // PY: test_reranker_config_deleted_factories_gone
  test("v0.3 fastembed factory is deleted", () => {
    const r = RerankerConfig as unknown as Record<string, unknown>;
    expect(typeof r.fastembed).toBe("undefined");
  });
});

describe("Lunaris.withEmbedder / withReranker — integration tier", () => {
  // PY: test_lunaris_open_with_embedder_kwarg
  test.skipIf(!IT_ENABLED)(
    "opens with custom embedder via withEmbedder()",
    async () => {
      const base = await Lunaris.open(MOON_URL);
      // noop() needs no model artifacts — the point of THIS test is the
      // withEmbedder plumbing, not the embedding math.
      const cfg = EmbedderConfig.noop();
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
  // Deferred to an upstream-supplied native model fixture (mirrors the
  // pytest sibling's deferral). The DimValidatingEmbedder wrapper IS active
  // in the BYO path — first embed_batch checks observed vs. declared dim —
  // but exercising it requires real model artifacts whose output dim ≠
  // declared. The Rust unit test
  // embedder_config.rs::tests::dim_validating_embedder_* covers the wrapper.
  test.skipIf(!IT_ENABLED)(
    "withEmbedder + bad dim raises on first embed (deferred — needs fixture)",
    () => {
      // Intentionally empty: documents the gap. Implementation lives in
      // open_overrides.rs::with_embedder + DimValidatingEmbedder.
      expect(true).toBe(true);
    },
  );
});
