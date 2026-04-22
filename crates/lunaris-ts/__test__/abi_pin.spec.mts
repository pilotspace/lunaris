// Plan 08-03 Task 1 — NAPI ABI pin assertion (ROADMAP Phase 8 risk-register row 4).
//
// The Rust side pins `napi = { features = ["napi8", ...] }` so the
// produced `.node` binary loads against NAPI v8 or higher. Node 20 LTS
// exposes NAPI 8; Node 18 caps at NAPI 7. This test asserts the runtime
// satisfies the pin before the rest of the suite runs — failing here
// gives a clearer error than "undefined symbol: napi_get_value_*" at
// binary load time.

import { describe, expect, test } from "vitest";

describe("NAPI ABI pin (BIND-TS-05 / ROADMAP Phase 8 risk register)", () => {
  test("process.versions.napi is at least 8 (Node 20+ stable ABI)", () => {
    // `process.versions.napi` is a string like "10" or "12" (higher = newer).
    // Pinning at 8 means anything Node 20 LTS and later is fine.
    const napi = process.versions.napi;
    expect(napi).toBeTypeOf("string");
    expect(Number.parseInt(napi, 10)).toBeGreaterThanOrEqual(8);
  });

  test("process.versions.node major is at least 20", () => {
    const node = process.versions.node;
    const major = Number.parseInt(node.split(".")[0], 10);
    expect(major).toBeGreaterThanOrEqual(20);
  });

  test("native binding loads cleanly", async () => {
    // Dynamic import so a load failure surfaces inside the test rather than
    // at suite bootstrap. If the .node file is built against the wrong NAPI
    // version, the Rust-side `napi_set_instance_data` calls fail at dlopen
    // time; we want vitest to report the exact reason.
    const mod = await import("../index.js");
    expect(mod).toBeDefined();
    expect(mod.Lunaris).toBeTypeOf("function");
    expect(mod.openHandle).toBeTypeOf("function");
  });
});
