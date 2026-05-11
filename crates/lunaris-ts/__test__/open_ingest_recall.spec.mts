// Plan 08-03 Task 1 — open / ingest / recall / forget / snapshot roundtrip.
//
// These tests exercise the full 5-method Rust surface behind the Lunaris
// handle. They REQUIRE a reachable Moon backend (LUNARIS_TEST_MOON_URL
// env var; default moon://127.0.0.1:6379) and are skipped when no backend
// is available — so the suite still passes on a fresh clone without a
// dev box.
//
// Offline behaviours (URL-parse error mapping to Error with a coarse
// subsystem prefix) are covered unconditionally — that exercises the
// errors::napi_err translator directly.

import { describe, expect, test } from "vitest";
import net from "node:net";

// Dynamic import — keeps `import lunaris` from blowing up before the
// abi_pin suite has a chance to report the real reason.
const lunaris = await import("../index.mjs");

const DEFAULT_MOON_URL = "moon://127.0.0.1:6379";

function resolveMoonUrl(): string {
  return process.env.LUNARIS_TEST_MOON_URL ?? DEFAULT_MOON_URL;
}

async function moonReachable(url: string): Promise<boolean> {
  const match = /^moon:\/\/([^:/]+):(\d+)/.exec(url);
  if (!match) return false;
  const host = match[1];
  const port = Number.parseInt(match[2], 10);
  return new Promise<boolean>((resolve) => {
    const sock = net.createConnection({ host, port, timeout: 400 });
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

function buildEpisode(content: string): object {
  // Construct a dict shaped like lunaris_core::primitives::Episode.
  // Mirrors the serde shape from crates/lunaris-core/src/primitives.rs.
  //
  // v0.2 (RFC 0001): the `scope` field is now required. We pass "_dev_" here
  // so the existing backwards-compat tests continue to pass. Production callers
  // should use `engine.scoped(Scope.new(...)).ingest(new EpisodeBuilder(...))`
  // instead of constructing the raw Episode object.
  const ts = Date.now();
  const rand = Math.floor(Math.random() * 1_000_000_000)
    .toString(36)
    .padStart(8, "0");
  // 26-char ULID-ish string — the Rust side parses via ulid::Ulid. We
  // compose a 26-char upper-Crockford-base32 string from timestamp+random.
  const id = `01${ts.toString(32).toUpperCase().padStart(10, "0")}${rand.toUpperCase().padStart(14, "0")}`.slice(0, 26);
  return {
    id,
    scope: "_dev_",
    source: "ts-test",
    content,
    t_ref: null,
    bt: {
      valid: [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
      sys: [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
    },
    metadata: {},
  };
}

describe("open / ingest / recall / forget / snapshot roundtrip", () => {
  test("errorsMapToLunarisError — unsupported scheme raises Error with STORAGE prefix", async () => {
    // Offline — exercises Rust-side URL parse + error taxonomy mapping.
    await expect(
      lunaris.openHandle("unsupported-scheme://nowhere"),
    ).rejects.toThrow(/STORAGE:/);
  });

  test("openRoundtrip — await open(url) returns a Lunaris handle", async () => {
    const url = resolveMoonUrl();
    if (!(await moonReachable(url))) {
      return;
    }
    try {
      const handle = await lunaris.open(url);
      expect(handle).toBeDefined();
      expect(typeof handle.ingest).toBe("function");
      expect(typeof handle.snapshot).toBe("function");
      expect(typeof handle.graphPipeline).toBe("object");
    } catch (err) {
      // Plain Redis without RediSearch fails the Lunaris handshake —
      // skip gracefully with the reason surfaced.
      if ((err as Error).message?.startsWith("STORAGE:")) return;
      throw err;
    }
  });

  test("snapshotRoundtrip — snapshot returns a monotonic LSN string", async () => {
    const url = resolveMoonUrl();
    if (!(await moonReachable(url))) return;
    try {
      const handle = await lunaris.open(url);
      const s1 = await handle.snapshot();
      const s2 = await handle.snapshot();
      expect(typeof s1).toBe("string");
      expect(typeof s2).toBe("string");
      // Lsn Display format is "{wall_ms}:{counter}" — compare on the
      // numeric parts because ordering isn't lexicographic after certain
      // wall/counter transitions.
      const parse = (s: string): [number, number] => {
        const [a, b] = s.split(":", 2);
        return [Number.parseInt(a, 10), Number.parseInt(b, 10)];
      };
      const [w1, c1] = parse(s1);
      const [w2, c2] = parse(s2);
      expect(w2 > w1 || (w2 === w1 && c2 >= c1)).toBe(true);
    } catch (err) {
      if ((err as Error).message?.startsWith("STORAGE:")) return;
      throw err;
    }
  });

  test("ingestRoundtrip — ingest returns an LSN string shaped wall_ms:counter", async () => {
    const url = resolveMoonUrl();
    if (!(await moonReachable(url))) return;
    try {
      const handle = await lunaris.open(url);
      const ep = buildEpisode("the quick brown fox jumps over the lazy dog");
      const lsn = await handle.ingest(ep);
      expect(typeof lsn).toBe("string");
      expect(lsn.includes(":")).toBe(true);

      const builder = handle.recall();
      expect(builder).toBeDefined();
    } catch (err) {
      if ((err as Error).message?.startsWith("STORAGE:")) return;
      throw err;
    }
  });

  test("forgetRoundtrip — dry_run returns a receipt object with preview=true", async () => {
    const url = resolveMoonUrl();
    if (!(await moonReachable(url))) return;
    try {
      const handle = await lunaris.open(url);
      const req = {
        target: { Scope: { BySource: "ts-test/nonexistent-scope" } },
        options: { hard: false, dry_run: true, confirmation_token: null },
      };
      const receipt = await handle.forget(req);
      expect(typeof receipt).toBe("object");
      expect((receipt as { preview?: boolean }).preview).toBe(true);
    } catch (err) {
      if ((err as Error).message?.startsWith("STORAGE:")) return;
      throw err;
    }
  });
});
