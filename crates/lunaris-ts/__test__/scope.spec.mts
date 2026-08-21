// Wave 3G — Scope / EpisodeBuilder / ScopedLunaris binding tests.
//
// Tests are split into two groups:
//
// 1. Offline — Scope validation + EpisodeBuilder construction. These run on
//    any machine, no backend required.
// 2. Online — ingest under a scope + cross-scope isolation. These require a
//    live Moon backend (LUNARIS_MOON_URL) and skip when none is
//    reachable.

import { describe, expect, test } from "vitest";
import { createConnection } from "node:net";

const lunaris = await import("../index.mjs");

const DEFAULT_MOON_URL = "moon://127.0.0.1:6380";

function resolveMoonUrl(): string {
  return process.env.LUNARIS_MOON_URL ?? DEFAULT_MOON_URL;
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

/**
 * Bail out of a test as SKIPPED, never as passed.
 *
 * A bare `return` from a test body is a PASS to vitest. Every test below
 * used to take one on an unreachable Moon AND on any error whose message
 * started with `STORAGE:` — so the cross-scope isolation test, the only
 * thing in this file proving one agent cannot read another's memories,
 * reported green whenever ingest or recall failed.
 */
type SkippableCtx = { skip: (reason?: string) => void };

/**
 * Probe, open, and hand back a handle — or SKIP with a named reason.
 *
 * The handshake is the ONLY place a `STORAGE:` error is tolerated, because
 * a plain Redis without RediSearch cannot open a Lunaris handle and that is
 * a missing prerequisite rather than a defect. Everything after the open is
 * the surface under test and must succeed or fail loudly.
 */
async function openOrSkip(ctx: SkippableCtx, url: string): Promise<ScopeHandle | null> {
  if (!(await moonReachable(url))) {
    ctx.skip(`Moon unreachable at ${url}`);
    return null;
  }
  try {
    return (await lunaris.open(url)) as ScopeHandle;
  } catch (err) {
    const msg = (err as Error).message ?? "";
    if (msg.startsWith("STORAGE:")) {
      ctx.skip(`the handle would not open (plain Redis without RediSearch?): ${msg}`);
      return null;
    }
    throw err;
  }
}

interface ScopedView {
  ingest: (b: unknown) => Promise<string>;
  recall: (q: string) => Promise<unknown>;
  dsl: () => unknown;
  scope: { asStr: () => string };
}

interface ScopeHandle {
  scoped: (s: unknown) => ScopedView;
}

describe("ScopedLunaris — online (requires Moon backend)", () => {
  test("scoped().ingest() returns an LSN string", async (ctx: SkippableCtx) => {
    const handle = await openOrSkip(ctx, resolveMoonUrl());
    if (!handle) return;
    const scope = lunaris.Scope.new("agent.alpha");
    const builder = new lunaris.EpisodeBuilder(
      "ts-test/scope",
      "the quick brown fox jumps over the lazy dog",
    );
    const scoped = handle.scoped(scope);
    const lsn = await scoped.ingest(builder);
    expect(typeof lsn).toBe("string");
    expect(lsn.includes(":")).toBe(true);
  });

  test("scoped().ingest() with metadata and tRef", async (ctx: SkippableCtx) => {
    const handle = await openOrSkip(ctx, resolveMoonUrl());
    if (!handle) return;
    const scope = lunaris.Scope.new("agent.alpha");
    const builder = new lunaris.EpisodeBuilder("ts-test/meta", "some content")
      .tRef("2026-01-01T00:00:00Z")
      .metadata({ source_type: "unit-test" });
    const lsn = await handle.scoped(scope).ingest(builder);
    expect(typeof lsn).toBe("string");
  });

  test("cross-scope isolation: scope_b does not see scope_a content", async (
    ctx: SkippableCtx,
  ) => {
    const handle = await openOrSkip(ctx, resolveMoonUrl());
    if (!handle) return;
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
  });

  test("scoped.scope getter returns bound Scope", async (ctx: SkippableCtx) => {
    const handle = await openOrSkip(ctx, resolveMoonUrl());
    if (!handle) return;
    const scope = lunaris.Scope.new("agent.alpha");
    const scoped = handle.scoped(scope);
    expect(scoped.scope.asStr()).toBe("agent.alpha");
  });

  test("scoped.dsl() returns a RetrievalBuilder", async (ctx: SkippableCtx) => {
    const handle = await openOrSkip(ctx, resolveMoonUrl());
    if (!handle) return;
    const scope = lunaris.Scope.new("agent.alpha");
    const builder = handle.scoped(scope).dsl();
    expect(builder).toBeDefined();
  });
});
