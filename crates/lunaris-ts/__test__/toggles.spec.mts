// Plan 08-03 Task 2 — BIND-TS-04 three-surface pipeline toggle tests.
//
// Three independent offline tests, one per surface:
//
// - Code: `handle.graphPipeline.enable()` — SKIPPED when no backend; the
//   class wrapper exists independently so we assert the method shape.
// - Env: `fromEnv()` reads `LUNARIS_GRAPH_ENABLED` / `LUNARIS_CONSOLIDATE_ENABLED`
//   directly — exercises the Rust-side `GraphPipelineHandle::initial_state_from_env`
//   path without requiring a handle / backend.
// - Config: `fromConfig({...})` mirrors the same decision shape — offline.
//
// The REMOVED crossLanguageHitParity test is intentionally NOT present here
// (WARNING 9 resolution — Plan 08-04 owns per-driver backend parity; no
// cross-language byte-identity is asserted anywhere).

import { describe, expect, test } from "vitest";

const {
  fromEnv,
  fromEnvValue,
  fromConfig,
  GraphPipelineHandleExt,
  ConsolidatorPipelineHandleExt,
} = await import("../index.mjs");

describe("three-surface pipeline toggles (BIND-TS-04)", () => {
  test("codeSurface — GraphPipelineHandleExt exposes enable / disable / isEnabled / toggle", () => {
    // Class-shape assertion — the concrete live-backend path is covered
    // by __test__/open_ingest_recall.spec.mts when a Moon dev box is reachable.
    expect(typeof GraphPipelineHandleExt).toBe("function");
    const proto = GraphPipelineHandleExt.prototype;
    expect(typeof proto.enable).toBe("function");
    expect(typeof proto.disable).toBe("function");
    expect(typeof proto.isEnabled).toBe("function");
    expect(typeof proto.toggle).toBe("function");

    expect(typeof ConsolidatorPipelineHandleExt).toBe("function");
    const protoC = ConsolidatorPipelineHandleExt.prototype;
    expect(typeof protoC.enable).toBe("function");
    expect(typeof protoC.disable).toBe("function");
    expect(typeof protoC.isEnabled).toBe("function");
    expect(typeof protoC.toggle).toBe("function");
  });

  test("envSurface — fromEnvValue() applies the Rust-side parse rules to explicit values", () => {
    // vitest's `process.env` Proxy layer does NOT propagate to `std::env::var`
    // in the Rust side during the same worker — calling `fromEnv()` after
    // `process.env.LUNARIS_GRAPH_ENABLED = "1"` returns `[false, false]` even
    // though `process.env.LUNARIS_GRAPH_ENABLED === "1"` in JS. The Rust-side
    // `from_env_value(Option<&str>)` pure helper is exposed as `fromEnvValue`
    // so we can assert the parse semantics independent of the env-mutation
    // propagation quirk. The real env-surface path (Lunaris::open reading
    // LUNARIS_GRAPH_ENABLED at construction time) is exercised when a
    // subprocess or CI runner sets the env BEFORE node starts.
    expect(fromEnvValue("1", "0")).toEqual([true, false]);
    expect(fromEnvValue("true", null)).toEqual([true, false]);
    expect(fromEnvValue("on", "TRUE")).toEqual([true, true]);
    expect(fromEnvValue("ON", null)).toEqual([true, false]);
    expect(fromEnvValue(null, null)).toEqual([false, false]);
    expect(fromEnvValue("no", "0")).toEqual([false, false]);
    expect(fromEnvValue(undefined, undefined)).toEqual([false, false]);
  });

  test("envSurface:live — fromEnv() is a live reader (returns defaults when env is unset at worker spawn)", () => {
    // Sanity check: fromEnv returns a [bool, bool] tuple. The actual
    // graph/consolidator value depends on whatever env the test runner
    // inherited — we only assert the shape here.
    const r = fromEnv();
    expect(Array.isArray(r)).toBe(true);
    expect(r.length).toBe(2);
    expect(typeof r[0]).toBe("boolean");
    expect(typeof r[1]).toBe("boolean");
  });

  test("configSurface — fromConfig({...}) defaults to false on missing keys (blueprint §5.1/§5.2 OFF default)", () => {
    const cfgOn = {
      graphPipeline: { enabled: true },
      consolidatorPipeline: { enabled: false },
    };
    const [g, c] = fromConfig(cfgOn);
    expect(g).toBe(true);
    expect(c).toBe(false);

    // Missing keys default to false.
    const [g2, c2] = fromConfig({});
    expect(g2).toBe(false);
    expect(c2).toBe(false);

    // Snake-case keys (shared with the Python config file) are also accepted.
    const [g3, c3] = fromConfig({ graph_pipeline: { enabled: true } });
    expect(g3).toBe(true);
    expect(c3).toBe(false);
  });
});
