// Plan 08-04 — per-driver backend parity (TypeScript driver).
//
// TypeScript-driver entry for the Phase 8 success-criterion #4 test:
// within a single process, ingest the FixtureCorpus via lunaris-ts's
// `conformanceFixtureEpisodes` + `handle.ingest` into BOTH Moon and
// Postgres backends (when env-configured), then scan each via
// `scanKvPrefix` and assert the normalized shape matches the committed
// golden reference at
// `crates/lunaris-conformance/fixtures/golden/bindings_fixture.json`.
//
// The Rust and Python drivers (`crates/lunaris-conformance/tests/
// run_bindings_backend_parity.rs` and `crates/lunaris-py/tests/
// test_backend_parity.py`) replicate this exact flow in their languages.
// Each driver's assertion is INDEPENDENT — no `expect(tsRows).toEqual(rustRows)`
// or equivalent is made in any of the three test bodies. Per-driver
// backend parity is the correct interpretation of success criterion #4
// (see `.planning/phases/08-sdk-bindings/08-04-PLAN.md` revision
// iteration 2 scope-reset note).
//
// Requires the `bindings-it` feature build (`napi build --platform
// --release --cargo-flags "--features bindings-it"`). When either env
// var is unset, that backend is skipped neutrally. When both are unset,
// the test exits without exercising the live paths — mirror of the Plan
// 04-03 / 05-02 two-tier skip pattern.

import { describe, expect, test } from "vitest";
import net from "node:net";
import fs from "node:fs";
import path from "node:path";
import url from "node:url";

// Dynamic import — keeps the binding load failure from blowing up before
// `abi_pin.spec.mts` has a chance to report the real reason.
const lunaris = await import("../index.mjs");

// Resolve the committed golden JSON relative to this spec. Structure:
//   crates/lunaris-ts/__test__/backend_parity.spec.mts  (this file)
//   crates/lunaris-conformance/fixtures/golden/bindings_fixture.json
// so we walk up 2 levels (__test__ → lunaris-ts → crates) then descend.
const __dirname = path.dirname(url.fileURLToPath(import.meta.url));
const GOLDEN_PATH = path.resolve(
  __dirname,
  "..",
  "..",
  "lunaris-conformance",
  "fixtures",
  "golden",
  "bindings_fixture.json",
);

interface GoldenReference {
  episode_count: number;
  chunk_count_per_episode: number;
  keys_prefix: string;
  embedding_dim: number;
  per_episode_key_count: number;
}

function loadGolden(): GoldenReference {
  const raw = fs.readFileSync(GOLDEN_PATH, "utf-8");
  return JSON.parse(raw) as GoldenReference;
}

// Canonical Rust constants — must match `lunaris_conformance::fixtures::SEED`
// + `EPISODE_COUNT`. The `conformanceFixtureEpisodes` FFI asserts these
// against the Rust constants at the boundary so silent cross-language
// drift surfaces as a hard napi::Error.
const SEED = BigInt(0xCAFE_F00D);
const COUNT = 10;

function parseHostPort(u: string): { host: string; port: number } | null {
  const schemes: [string, number][] = [
    ["moon://", 6379],
    ["postgres://", 5432],
    ["postgresql://", 5432],
  ];
  for (const [scheme, defaultPort] of schemes) {
    if (u.startsWith(scheme)) {
      let rest = u.slice(scheme.length);
      // postgres URLs can carry userinfo — strip it before splitting.
      const atIdx = rest.lastIndexOf("@");
      if (atIdx >= 0) {
        rest = rest.slice(atIdx + 1);
      }
      const authority = rest.split("/")[0].split("?")[0];
      const colonIdx = authority.indexOf(":");
      if (colonIdx < 0) return { host: authority, port: defaultPort };
      const host = authority.slice(0, colonIdx);
      const port = Number.parseInt(authority.slice(colonIdx + 1), 10);
      if (!Number.isFinite(port)) return null;
      return { host, port };
    }
  }
  return null;
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

async function probeBackend(envName: string): Promise<string | null> {
  const u = process.env[envName];
  if (!u) {
    // eslint-disable-next-line no-console
    console.error(`backend_parity: SKIP ${envName} (env var unset)`);
    return null;
  }
  const parsed = parseHostPort(u);
  if (!parsed) {
    // eslint-disable-next-line no-console
    console.error(`backend_parity: SKIP ${envName} (unknown URL scheme or malformed)`);
    return null;
  }
  if (!(await reachable(parsed.host, parsed.port))) {
    // Intentionally do NOT log the full URL — postgres:// URLs can
    // carry credentials in the userinfo segment (T-05-02-01 mitigation,
    // mirror of Rust-side probe_backend).
    // eslint-disable-next-line no-console
    console.error(
      `backend_parity: SKIP ${envName} (TCP probe to ${parsed.host}:${parsed.port} failed)`,
    );
    return null;
  }
  return u;
}

interface NormalizedRows {
  total_rows: number;
  keys_prefix: string;
  distinct_episode_ids: number;
}

async function ingestAndNormalize(
  u: string,
  golden: GoldenReference,
): Promise<NormalizedRows> {
  // `conformanceFixtureEpisodes` asserts the SEED + COUNT match the
  // Rust constants; drift raises `napi::Error(InvalidArg)` at the FFI
  // boundary.
  const episodes = lunaris.conformanceFixtureEpisodes(SEED, COUNT) as object[];
  expect(episodes).toHaveLength(COUNT);

  const handle = await lunaris.open(u);
  for (const ep of episodes) {
    await handle.ingest(ep);
  }

  const prefixBuf = Buffer.from(golden.keys_prefix, "utf-8");
  const rawRows = (await lunaris.scanKvPrefix(handle, prefixBuf)) as [Buffer, Buffer][];

  // Parse each key as "{prefix}{ulid}" and collect unique suffixes.
  const distinct = new Set<string>();
  for (const [k] of rawRows) {
    let keyStr: string;
    try {
      keyStr = Buffer.from(k).toString("utf-8");
    } catch {
      continue;
    }
    if (keyStr.startsWith(golden.keys_prefix)) {
      distinct.add(keyStr.slice(golden.keys_prefix.length));
    }
  }

  return {
    total_rows: rawRows.length,
    keys_prefix: golden.keys_prefix,
    distinct_episode_ids: distinct.size,
  };
}

function assertStructuralEq(
  rows: NormalizedRows,
  golden: GoldenReference,
  label: string,
): void {
  // Mirror of `lunaris_conformance::bindings::assert_structural_eq`.
  // Fails with a human-readable reason naming the backend so CI logs
  // surface the cause.
  expect(
    rows.keys_prefix,
    `${label} prefix drift: got ${JSON.stringify(rows.keys_prefix)} want ${JSON.stringify(golden.keys_prefix)}`,
  ).toBe(golden.keys_prefix);
  expect(
    rows.distinct_episode_ids,
    `${label} episode count drift: got ${rows.distinct_episode_ids} want ${golden.episode_count}`,
  ).toBe(golden.episode_count);
  const wantTotal = golden.episode_count * golden.per_episode_key_count;
  expect(
    rows.total_rows,
    `${label} row-count drift: got ${rows.total_rows} want ${wantTotal} (${golden.episode_count} episodes × ${golden.per_episode_key_count} keys/episode)`,
  ).toBe(wantTotal);
}

function bindingsItFeaturePresent(): boolean {
  // Production `.node` builds do NOT ship `conformanceFixtureEpisodes` /
  // `scanKvPrefix`; Plan 08-04 only runs this test against a `bindings-it`
  // build. When the helpers are absent, skip the test with a clear reason.
  return (
    typeof (lunaris as Record<string, unknown>).conformanceFixtureEpisodes === "function" &&
    typeof (lunaris as Record<string, unknown>).scanKvPrefix === "function"
  );
}

describe("Plan 08-04 — per-driver backend parity (TypeScript)", () => {
  test("goldenReferenceLoads — committed JSON has the expected shape", () => {
    // Offline check (no backend needed). Guards against accidental drift
    // in the committed JSON.
    const golden = loadGolden();
    for (const required of [
      "episode_count",
      "chunk_count_per_episode",
      "keys_prefix",
      "embedding_dim",
      "per_episode_key_count",
    ]) {
      expect(golden).toHaveProperty(required);
    }
    expect(golden.episode_count).toBe(COUNT);
    expect(golden.keys_prefix).toBe("lunaris:_dev_:episode:");
    expect(golden.per_episode_key_count).toBe(1);
  });

  test("tsDriverBackendParity — ingest + scan + assert against golden", async () => {
    if (!bindingsItFeaturePresent()) {
      // vitest can't skip mid-test; exit as a pass (no-op) with a
      // reason printed to the console — mirrors the Plan 08-03
      // backend-unreachable no-op style.
      // eslint-disable-next-line no-console
      console.error(
        "backend_parity: SKIP (lunaris-ts built without `bindings-it` feature — " +
          "run `napi build --platform --release --cargo-flags \"--features bindings-it\"` " +
          "to build the conformance helpers)",
      );
      return;
    }

    const golden = loadGolden();
    const moonUrl = await probeBackend("LUNARIS_MOON_URL");
    const pgUrl = await probeBackend("LUNARIS_POSTGRES_URL");

    if (!moonUrl && !pgUrl) {
      // Neither backend reachable — skip neutrally (no-op pass).
      // eslint-disable-next-line no-console
      console.error(
        "backend_parity: SKIP (neither LUNARIS_MOON_URL nor LUNARIS_POSTGRES_URL configured with a reachable backend)",
      );
      return;
    }

    if (moonUrl) {
      const rows = await ingestAndNormalize(moonUrl, golden);
      assertStructuralEq(rows, golden, "moon");
    }
    if (pgUrl) {
      const rows = await ingestAndNormalize(pgUrl, golden);
      assertStructuralEq(rows, golden, "postgres");
    }
  }, 60_000);
});
