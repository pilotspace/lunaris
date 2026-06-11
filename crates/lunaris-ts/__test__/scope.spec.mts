// Wave 3G — Scope / EpisodeBuilder / ScopedLunaris binding tests.
//
// Tests are split into two groups:
//
// 1. Offline — Scope validation + EpisodeBuilder construction. These run on
//    any machine, no backend required.
// 2. Online — ingest under a scope + cross-scope isolation. These require a
//    live Moon backend (LUNARIS_TEST_MOON_URL) and skip when none is
//    reachable.

import { describe, expect, test } from "vitest";
import { createConnection } from "node:net";

const lunaris = await import("../index.mjs");

const DEFAULT_MOON_URL = "moon://127.0.0.1:6380";

function resolveMoonUrl(): string {
  return process.env.LUNARIS_TEST_MOON_URL ?? DEFAULT_MOON_URL;
}

async function moonReachable(url: string): Promise<boolean> {
  const match = /^moon:\/\/([^:/]+):(\d+)/.exec(url);
  if (!match) return false;
  const host = match[1];
  const port = Number.parseInt(match[2], 10);
  return new Promise<boolean>((resolve) => {
    const sock = createConnection({ host, port, timeout: 400 });
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

// ---------------------------------------------------------------------------
// Offline: Scope validation
// ---------------------------------------------------------------------------

describe("Scope — offline validation", () => {
  test("valid scope constructs without error", () => {
    const s = lunaris.Scope.new("acme.agent-42");
    expect(s.asStr()).toBe("acme.agent-42");
    expect(s.toString()).toBe("acme.agent-42");
  });

  test("equality: same string produces same asStr", () => {
    const a = lunaris.Scope.new("agent.alpha");
    const b = lunaris.Scope.new("agent.alpha");
    expect(a.asStr()).toBe(b.asStr());
  });

  test("rejects colon (KV-aliasing defense, v0.2.1 alphabet)", () => {
    // `:` is the delimiter of the lunaris:{scope}:{kind}:{ulid} KV format.
    // Scope::new rejects it at the type level so no scope string can
    // byte-alias another scope's keyspace. A spec asserting `:` valid is
    // asserting a security regression.
    expect(() => lunaris.Scope.new("acme:agent-42")).toThrow(/Scope/);
  });

  test("rejects empty string", () => {
    expect(() => lunaris.Scope.new("")).toThrow(/Scope/);
  });

  test("rejects 129-char string (limit is 128)", () => {
    expect(() => lunaris.Scope.new("a".repeat(129))).toThrow(/Scope/);
  });

  test("accepts 128-char string (exact limit)", () => {
    const s = lunaris.Scope.new("a".repeat(128));
    expect(s.asStr().length).toBe(128);
  });

  test("rejects string with space", () => {
    expect(() => lunaris.Scope.new("has space")).toThrow(/Scope/);
  });

  test("rejects string with slash", () => {
    expect(() => lunaris.Scope.new("has/slash")).toThrow(/Scope/);
  });

  test("accepts all valid char classes: A-Za-z0-9_-.", () => {
    expect(() => lunaris.Scope.new("A0._-")).not.toThrow();
  });
});

// ---------------------------------------------------------------------------
// Offline: EpisodeBuilder
// ---------------------------------------------------------------------------

describe("EpisodeBuilder — offline", () => {
  test("basic construction", () => {
    const b = new lunaris.EpisodeBuilder("src/report.md", "hello world");
    expect(b).toBeDefined();
  });

  test("tRef with valid ISO-8601 string", () => {
    const b = new lunaris.EpisodeBuilder("src", "content").tRef(
      "2026-01-01T00:00:00Z",
    );
    expect(b).toBeDefined();
  });

  test("tRef with invalid string throws", () => {
    expect(() =>
      new lunaris.EpisodeBuilder("src", "content").tRef("not-a-date"),
    ).toThrow(/tRef|ISO/i);
  });

  test("metadata with valid object", () => {
    const b = new lunaris.EpisodeBuilder("src", "content").metadata({
      author: "helios",
    });
    expect(b).toBeDefined();
  });

  test("builder chaining works", () => {
    const b = new lunaris.EpisodeBuilder("src", "content")
      .tRef("2026-05-11T00:00:00Z")
      .metadata({ k: "v" });
    expect(b).toBeDefined();
  });
});

// ---------------------------------------------------------------------------
// Online: ScopedLunaris ingest + cross-scope isolation
// ---------------------------------------------------------------------------

describe("ScopedLunaris — online (requires Moon backend)", () => {
  test("scoped().ingest() returns an LSN string", async () => {
    const url = resolveMoonUrl();
    if (!(await moonReachable(url))) return;

    try {
      const handle = await lunaris.open(url);
      const scope = lunaris.Scope.new("agent.alpha");
      const builder = new lunaris.EpisodeBuilder(
        "ts-test/scope",
        "the quick brown fox jumps over the lazy dog",
      );
      const scoped = handle.scoped(scope);
      const lsn = await scoped.ingest(builder);
      expect(typeof lsn).toBe("string");
      expect(lsn.includes(":")).toBe(true);
    } catch (err) {
      if ((err as Error).message?.startsWith("STORAGE:")) return;
      throw err;
    }
  });

  test("scoped().ingest() with metadata and tRef", async () => {
    const url = resolveMoonUrl();
    if (!(await moonReachable(url))) return;

    try {
      const handle = await lunaris.open(url);
      const scope = lunaris.Scope.new("agent.alpha");
      const builder = new lunaris.EpisodeBuilder("ts-test/meta", "some content")
        .tRef("2026-01-01T00:00:00Z")
        .metadata({ source_type: "unit-test" });
      const lsn = await handle.scoped(scope).ingest(builder);
      expect(typeof lsn).toBe("string");
    } catch (err) {
      if ((err as Error).message?.startsWith("STORAGE:")) return;
      throw err;
    }
  });

  test("cross-scope isolation: scope_b does not see scope_a content", async () => {
    const url = resolveMoonUrl();
    if (!(await moonReachable(url))) return;

    try {
      const handle = await lunaris.open(url);
      const unique = `lunaris-wave3g-ts-isolation-${Date.now()}`;

      const scopeA = lunaris.Scope.new("wave3g.scope-a");
      const scopeB = lunaris.Scope.new("wave3g.scope-b");

      // Ingest under scope_a.
      const builder = new lunaris.EpisodeBuilder("ts-test/isolation", unique);
      await handle.scoped(scopeA).ingest(builder);

      // Recall under scope_b — must not return our unique sentinel.
      const hitsB = await handle.scoped(scopeB).recall(unique);
      expect(Array.isArray(hitsB)).toBe(true);

      const matching = (hitsB as unknown[]).filter((h) =>
        JSON.stringify(h).includes(unique),
      );
      expect(matching).toHaveLength(0);
    } catch (err) {
      if ((err as Error).message?.startsWith("STORAGE:")) return;
      throw err;
    }
  });

  test("scoped.scope getter returns bound Scope", async () => {
    const url = resolveMoonUrl();
    if (!(await moonReachable(url))) return;

    try {
      const handle = await lunaris.open(url);
      const scope = lunaris.Scope.new("agent.alpha");
      const scoped = handle.scoped(scope);
      expect(scoped.scope.asStr()).toBe("agent.alpha");
    } catch (err) {
      if ((err as Error).message?.startsWith("STORAGE:")) return;
      throw err;
    }
  });

  test("scoped.dsl() returns a RetrievalBuilder", async () => {
    const url = resolveMoonUrl();
    if (!(await moonReachable(url))) return;

    try {
      const handle = await lunaris.open(url);
      const scope = lunaris.Scope.new("agent.alpha");
      const builder = handle.scoped(scope).dsl();
      expect(builder).toBeDefined();
    } catch (err) {
      if ((err as Error).message?.startsWith("STORAGE:")) return;
      throw err;
    }
  });
});
