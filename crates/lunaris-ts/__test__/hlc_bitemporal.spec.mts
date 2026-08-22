// Defect F20 — the TypeScript SDK could not construct an `Hlc`.
//
// WHAT THIS FILE IS FOR
// ---------------------
// `lunaris_core::json_repair` is unit tested in Rust, but a green unit test
// only proves the repair WORKS — not that the shipped `.node` binary routes
// through it. This suite crosses the real napi FFI boundary, which is the
// only place the defect ever existed: napi's own
// `impl FromNapiValue for serde_json::Number` is what drops integer-ness, and
// no Rust-side test can reproduce that conversion for real, only replicate it.
//
// THE DEFECT
// ----------
// napi 3.8.5 preserves integer-ness only for values that fit in `u32` or
// `i32`; everything larger becomes `Number::from_f64`. The boundary is
// `u32::MAX` = 4_294_967_295 — every millisecond timestamp since 1970-02-19.
// So an ordinary JavaScript `{ wall_ms: 1736467200000, counter: 0, node_id: 0 }`
// arrived as `1736467200000.0` and the generated binding's
// `serde_json::from_value::<Hlc>` rejected it:
//
//     VALIDATE: invalid type: floating point `1736467200000.0`, expected u64
//
// `.between()` and `.as_of()` — the entire bi-temporal time-travel surface —
// were unusable from TypeScript. `counter` and `node_id`, being small,
// converted fine; that asymmetry is why it went unnoticed for so long.
//
// WHY THIS IS NOT FOLDED INTO documentary_parity.spec.mts
// -------------------------------------------------------
// That file's `.between()` scenario is blocked by a SECOND, independent
// defect — F21, `TimelineReconstruction.ingest` cannot set a historical
// valid-time, so a historical window returns zero rows even from Python where
// the `Hlc` converts fine. A test that asserts on ROWS therefore cannot tell
// F20 from F21.
//
// These tests assert on the CALL, not on the rows: F20 made the call THROW,
// F21 only makes it return an empty array. So they stay green when F21 is
// fixed, red if F20 ever regresses, and they say which one is which. The
// row-level assertions remain F21's to restore.

import { describe, expect, test } from "vitest";
import net from "node:net";

const lunaris = await import("../index.mjs");

// A real millisecond timestamp: 2025-01-10T00:00:00Z. Chosen because it is
// past `u32::MAX` — the entire point — and exactly representable in an f64,
// so a value mismatch could only come from the repair, never from JS.
const MS_2025_01_10 = 1_736_467_200_000;
const MS_2025_01_16 = 1_736_985_600_000;

/** `Hlc` as an ordinary JS object literal — the shape a caller would write. */
function hlc(wallMs: number) {
  return { wall_ms: wallMs, counter: 0, node_id: 0 };
}

type SkippableCtx = { skip: (reason?: string) => void };

function parseHostPort(u: string): { host: string; port: number } | null {
  if (!u.startsWith("moon://")) return null;
  let rest = u.slice("moon://".length);
  const atIdx = rest.lastIndexOf("@");
  if (atIdx >= 0) rest = rest.slice(atIdx + 1);
  const authority = rest.split("/")[0].split("?")[0];
  const colonIdx = authority.indexOf(":");
  if (colonIdx < 0) return { host: authority, port: 6379 };
  const port = Number.parseInt(authority.slice(colonIdx + 1), 10);
  if (!Number.isFinite(port)) return null;
  return { host: authority.slice(0, colonIdx), port };
}

async function reachable(host: string, port: number): Promise<boolean> {
  return new Promise<boolean>((resolve) => {
    const sock = net.createConnection({ host, port, timeout: 1000 });
    sock.once("connect", () => {
      sock.end();
      resolve(true);
    });
    sock.once("error", () => resolve(false));
    sock.once("timeout", () => {
      sock.destroy();
      resolve(false);
    });
  });
}

async function moonOrNull(): Promise<string | null> {
  const u = process.env.LUNARIS_MOON_URL;
  if (!u) return null;
  const parsed = parseHostPort(u);
  if (parsed === null) return null;
  return (await reachable(parsed.host, parsed.port)) ? u : null;
}

/**
 * F20 was a TYPE-CONVERSION failure, and it surfaced as a `VALIDATE:` napi
 * error naming a floating-point value. Any other failure — Moon down, an
 * empty result set, a recipe-level gap — is a different defect and must not
 * be able to make these tests green or red.
 */
function assertNotAnHlcConversionFailure(err: unknown, what: string): void {
  const msg = err instanceof Error ? err.message : String(err);
  expect(
    msg,
    `${what} rejected the Hlc that JavaScript can actually produce — F20 has ` +
      `regressed. Check that the generated bindings still call ` +
      `from_js(...) and not serde_json::from_value(...): ${msg}`,
  ).not.toMatch(/floating point/i);
  // Restated positively so a reworded serde message cannot quietly pass.
  expect(msg, `${what} raised a VALIDATE type error: ${msg}`).not.toMatch(
    /^VALIDATE:.*expected (u64|i64|u32|u16)/,
  );
}

describe("F20 — a JS millisecond timestamp survives the napi boundary", () => {
  test("the fixture timestamps are past the u32 boundary that broke", () => {
    // If this ever fails the rest of the suite is testing nothing: below
    // u32::MAX napi preserves integer-ness and the defect cannot reproduce.
    expect(MS_2025_01_10).toBeGreaterThan(4_294_967_295);
    expect(MS_2025_01_16).toBeGreaterThan(4_294_967_295);
    expect(Number.isSafeInteger(MS_2025_01_10)).toBe(true);
    expect(Number.isSafeInteger(MS_2025_01_16)).toBe(true);
  });

  test("TimelineReconstruction.between accepts an Hlc built in JS", async (ctx: SkippableCtx) => {
    const moon = await moonOrNull();
    if (moon === null) {
      ctx.skip("LUNARIS_MOON_URL unset or unreachable");
      return;
    }
    const handle = await lunaris.open(moon);
    const timeline = lunaris.TimelineReconstruction.new(handle, "f20-between:");

    // Seed one event so the query runs against a populated index rather than
    // an empty-index edge case, and so the query term survives Moon's
    // analyzer (a stopword-only query is rejected before it reaches the
    // bi-temporal filter, which would test nothing).
    await timeline.ingest([["deployment rollout milestone", { kind: "release" }]]);

    // The assertion is on the CALL surviving, not on the rows: F21 (the
    // recipe cannot store a historical valid-time) legitimately makes this
    // empty, and that is a different defect with its own guard.
    let rows: unknown[] | undefined;
    try {
      rows = await timeline.between("deployment", hlc(MS_2025_01_10), hlc(MS_2025_01_16));
    } catch (err) {
      assertNotAnHlcConversionFailure(err, "TimelineReconstruction.between");
      throw err;
    }
    expect(Array.isArray(rows)).toBe(true);
  }, 60_000);

  test("TimelineReconstruction.asOf accepts an Hlc built in JS", async (ctx: SkippableCtx) => {
    const moon = await moonOrNull();
    if (moon === null) {
      ctx.skip("LUNARIS_MOON_URL unset or unreachable");
      return;
    }
    const handle = await lunaris.open(moon);
    const timeline = lunaris.TimelineReconstruction.new(handle, "f20-asof:");
    await timeline.ingest([["deployment rollout milestone", { kind: "release" }]]);

    try {
      const rows = await timeline.asOf("deployment", hlc(Date.now()));
      expect(Array.isArray(rows)).toBe(true);
    } catch (err) {
      assertNotAnHlcConversionFailure(err, "TimelineReconstruction.asOf");
      // Moon refuses historical KV reads outside a 1 h live window
      // (`HISTORICAL_KV_READS = false`), which is a STORAGE error, not a
      // conversion one. `Date.now()` is inside that window, but a slow box
      // could still trip a different storage path — rethrow so it is visible
      // rather than swallowed.
      throw err;
    }
  }, 60_000);

  test("CodeRepoMemory.ingestCommit accepts a JS millisecond timestamp", async (ctx: SkippableCtx) => {
    const moon = await moonOrNull();
    if (moon === null) {
      ctx.skip("LUNARIS_MOON_URL unset or unreachable");
      return;
    }
    const handle = await lunaris.open(moon);
    const repo = lunaris.CodeRepoMemory.new(handle, "f20-commit:");

    // F20 is not an `Hlc`-only defect: `committerDateUnixMs` lowers into an
    // `i64` and broke for exactly the same reason. This is the second,
    // independent lowering path through the same repair.
    try {
      await repo.ingestCommit("f20cafe", MS_2025_01_10, [
        ["fn f20() -> u64 { 1736467200000 }", { path: "src/f20.rs" }],
      ]);
    } catch (err) {
      assertNotAnHlcConversionFailure(err, "CodeRepoMemory.ingestCommit");
      throw err;
    }
  }, 60_000);

  test("a genuinely malformed Hlc is still rejected", async (ctx: SkippableCtx) => {
    const moon = await moonOrNull();
    if (moon === null) {
      ctx.skip("LUNARIS_MOON_URL unset or unreachable");
      return;
    }
    const handle = await lunaris.open(moon);
    const timeline = lunaris.TimelineReconstruction.new(handle, "f20-bad:");

    // The repair must not have turned the binding into a shape-blind sink.
    // A fractional wall_ms is not an integer in any encoding and must fail.
    await expect(
      timeline.between("deployment", { wall_ms: 1.5, counter: 0, node_id: 0 }, hlc(MS_2025_01_16)),
    ).rejects.toThrow(/VALIDATE/);

    // A missing field must fail too.
    await expect(
      timeline.between("deployment", { counter: 0, node_id: 0 }, hlc(MS_2025_01_16)),
    ).rejects.toThrow(/VALIDATE/);

    // And a value past u64 must NOT be silently saturated into range — the
    // repair leaves out-of-range magnitudes as floats precisely so serde
    // still rejects them.
    await expect(
      timeline.between("deployment", { wall_ms: 1e30, counter: 0, node_id: 0 }, hlc(MS_2025_01_16)),
    ).rejects.toThrow(/VALIDATE/);
  }, 60_000);
});
