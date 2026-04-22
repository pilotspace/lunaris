// Phase 10 Plan 10-03 — TypeScript cross-language parity for conversational wrappers.
//
// Drives each of the 5 Phase 10 conversational wrappers end-to-end from the
// napi-rs binding layer (ChatAgentMemory, MultiTurnConversation, SlackArchive,
// EmailThreading, MeetingNotesMemory) through BOTH Moon and Postgres,
// re-using the canonical JSON fixtures landed by Plan 10-01 under
// `crates/lunaris-recipes/tests/fixtures/conversational/`.
//
// Per-driver backend parity — direct Moon-vs-Postgres hit-id + hit-source
// compare — mirrors the scope-reset Phase 8 Plan 08-04 convention AND the
// Plan 10-01 Rule 1 deviation (no pre-computed SHA-256 fixture lock, no
// cross-language byte-identity assertion).
//
// Skip posture: both LUNARIS_TEST_MOON_URL + LUNARIS_TEST_POSTGRES_URL must
// be set and TCP-reachable for the 6 live-ingest scenarios to fire.
// `vitest` can't mid-test-skip so when backends are absent the test body
// exits as a no-op pass with a SKIP reason printed to stderr — mirror of
// Plan 08-04's `backend_parity.spec.mts` style.
//
// Naming: `__test__/` (singular per crate-local vitest config include
// `__test__/**/*.spec.mts`). Plan 10-03 frontmatter says `__tests__/` —
// Rule 1 deviation; the on-disk directory is `__test__/` and it's the
// vitest glob that actually runs.
//
// Import path: `from 'lunaris'` flat crate root — the 11-02b Known
// Limitation documented `lunaris/conversational` subpath as unsupported
// under napi-rs 3.x; Plan 10-03 Wave C confirms the flat import works
// against the index.mjs re-exports landed in the same wave.

import { describe, expect, test } from "vitest";
import net from "node:net";
import fs from "node:fs";
import path from "node:path";
import url from "node:url";

// Dynamic import — matches the `backend_parity.spec.mts` pattern so a
// binding-load failure surfaces via `abi_pin.spec.mts` first.
const lunaris = await import("../index.mjs");

// Resolve the committed fixture JSON relative to this spec. Structure:
//   crates/lunaris-ts/__test__/conversational_parity.spec.mts  (this file)
//   crates/lunaris-recipes/tests/fixtures/conversational/*.json
// so we walk up 2 levels (__test__ → lunaris-ts → crates) then descend
// into lunaris-recipes.
const __dirname = path.dirname(url.fileURLToPath(import.meta.url));
const FIXTURE_DIR = path.resolve(
  __dirname,
  "..",
  "..",
  "lunaris-recipes",
  "tests",
  "fixtures",
  "conversational",
);

function loadFixture(name: string): Record<string, unknown> {
  const p = path.join(FIXTURE_DIR, name);
  const raw = fs.readFileSync(p, "utf-8");
  return JSON.parse(raw) as Record<string, unknown>;
}

function parseHostPort(u: string): { host: string; port: number } | null {
  const schemes: [string, number][] = [
    ["moon://", 6379],
    ["postgres://", 5432],
    ["postgresql://", 5432],
  ];
  for (const [scheme, defaultPort] of schemes) {
    if (u.startsWith(scheme)) {
      let rest = u.slice(scheme.length);
      const atIdx = rest.lastIndexOf("@");
      if (atIdx >= 0) rest = rest.slice(atIdx + 1);
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
    console.error(`conversational_parity: SKIP ${envName} (env var unset)`);
    return null;
  }
  const parsed = parseHostPort(u);
  if (!parsed) {
    // eslint-disable-next-line no-console
    console.error(`conversational_parity: SKIP ${envName} (unknown URL scheme)`);
    return null;
  }
  if (!(await reachable(parsed.host, parsed.port))) {
    // eslint-disable-next-line no-console
    console.error(
      `conversational_parity: SKIP ${envName} (TCP probe to ${parsed.host}:${parsed.port} failed)`,
    );
    return null;
  }
  return u;
}

/** Acquire both backend URLs, or `null` to indicate skip. */
async function requireBothBackends(): Promise<{ moon: string; pg: string } | null> {
  const moon = await probeBackend("LUNARIS_TEST_MOON_URL");
  const pg = await probeBackend("LUNARIS_TEST_POSTGRES_URL");
  if (!moon || !pg) {
    // eslint-disable-next-line no-console
    console.error(
      "conversational_parity: SKIP (requires BOTH LUNARIS_TEST_MOON_URL + LUNARIS_TEST_POSTGRES_URL)",
    );
    return null;
  }
  return { moon, pg };
}

interface Hit {
  id: number[];
  source: string;
  [k: string]: unknown;
}

function hitKey(hit: Hit): string {
  // Stable ordering key on a napi-rs-deserialised Hit — `(source, id)`.
  // `id` is a JSON byte array. Stringify for tuple equality.
  return `${hit.source}::${(hit.id || []).join(",")}`;
}

function compareHits(label: string, moon: Hit[], pg: Hit[]): string[] {
  const divs: string[] = [];
  if (moon.length !== pg.length) {
    divs.push(`${label}: hit_count moon=${moon.length} postgres=${pg.length}`);
  }
  const n = Math.min(moon.length, pg.length);
  for (let i = 0; i < n; i++) {
    const mk = hitKey(moon[i]);
    const pk = hitKey(pg[i]);
    if (mk !== pk) {
      divs.push(`${label} position ${i}: id/source moon=${mk} postgres=${pk}`);
    }
  }
  return divs;
}

/** Open both handles or null-skip on handshake failure (plain Redis without RediSearch
 *  trips `FT.CREATE` — mirror of the Py-side `_probe_handshake`). */
async function openPair(
  moonUrl: string,
  pgUrl: string,
): Promise<{ moon: unknown; pg: unknown } | null> {
  let moon: unknown;
  let pg: unknown;
  try {
    moon = await (lunaris as { open: (u: string) => Promise<unknown> }).open(moonUrl);
  } catch (err) {
    // eslint-disable-next-line no-console
    console.error(
      `conversational_parity: SKIP (Moon handshake failed: ${(err as Error).message})`,
    );
    return null;
  }
  try {
    pg = await (lunaris as { open: (u: string) => Promise<unknown> }).open(pgUrl);
  } catch (err) {
    // eslint-disable-next-line no-console
    console.error(
      `conversational_parity: SKIP (Postgres handshake failed: ${(err as Error).message})`,
    );
    return null;
  }
  return { moon, pg };
}

describe("Plan 10-03 — TypeScript conversational parity (dual-backend)", () => {
  test("surface imports — all 5 conversational classes resolve flat from 'lunaris'", () => {
    // Offline shape check — guards against 11-02b's re-export regressing
    // under a future refactor. Mirror of the Py-side test.
    const names = [
      "ChatAgentMemory",
      "MultiTurnConversation",
      "SlackArchive",
      "SlackArchiveQuery",
      "EmailThreading",
      "MeetingNotesMemory",
      "MeetingNotesQuery",
    ];
    for (const n of names) {
      expect(
        typeof (lunaris as Record<string, unknown>)[n],
        `${n} must be exported from 'lunaris' crate root`,
      ).toBe("function");
    }
  });

  test("fixtures on disk — all 5 conversational fixtures present", () => {
    for (const f of [
      "chat_agent_memory.json",
      "multi_turn_conversation.json",
      "slack_archive.json",
      "email_threading.json",
      "meeting_notes_memory.json",
    ]) {
      const p = path.join(FIXTURE_DIR, f);
      expect(fs.existsSync(p), `fixture missing: ${p}`).toBe(true);
    }
  });

  test("chat_agent_memory parity — 10-turn chat Moon-vs-Postgres", async () => {
    const both = await requireBothBackends();
    if (!both) return;
    const pair = await openPair(both.moon, both.pg);
    if (!pair) return;
    const fixture = loadFixture("chat_agent_memory.json");
    const userId = fixture.user_id as string;
    const turns = fixture.turns as { text: string }[];
    const query = fixture.query as string;
    const { ChatAgentMemory } = lunaris as {
      ChatAgentMemory: new (h: unknown, userId: string) => {
        remember: (t: string) => Promise<string>;
        recall: (q: string) => Promise<Hit[]>;
      };
    };
    const camMoon = new ChatAgentMemory(pair.moon, userId);
    const camPg = new ChatAgentMemory(pair.pg, userId);
    for (const turn of turns) {
      await camMoon.remember(turn.text);
      await camPg.remember(turn.text);
    }
    const moonHits = await camMoon.recall(query);
    const pgHits = await camPg.recall(query);
    const divs = compareHits("chat_agent_memory", moonHits, pgHits);
    expect(divs, `ChatAgentMemory parity: ${divs.join("; ")}`).toEqual([]);
  });

  test("multi_turn_conversation parity — recall + consolidate report parity", async () => {
    const both = await requireBothBackends();
    if (!both) return;
    const pair = await openPair(both.moon, both.pg);
    if (!pair) return;
    const fixture = loadFixture("multi_turn_conversation.json");
    const userId = fixture.user_id as string;
    const otherUserId = fixture.other_user_id as string;
    const sessions = fixture.sessions as {
      thread_id: string;
      turns: { text: string }[];
    }[];
    const otherTurns = fixture.other_turns as { text: string }[];
    const query = fixture.query as string;
    const { MultiTurnConversation } = lunaris as {
      MultiTurnConversation: new (h: unknown, userId: string) => {
        remember: (t: string, threadId: string) => Promise<string>;
        recall: (q: string) => Promise<Hit[]>;
        consolidate: () => Promise<{ promotions?: unknown[]; archives?: unknown[] }>;
      };
    };
    const convMoon = new MultiTurnConversation(pair.moon, userId);
    const convPg = new MultiTurnConversation(pair.pg, userId);
    const otherMoon = new MultiTurnConversation(pair.moon, otherUserId);
    const otherPg = new MultiTurnConversation(pair.pg, otherUserId);
    for (const s of sessions) {
      for (const t of s.turns) {
        await convMoon.remember(t.text, s.thread_id);
        await convPg.remember(t.text, s.thread_id);
      }
    }
    // Scope-isolation control seed — default-NoopConsolidator path on the
    // Py/TS bindings (set_consolidator is NOT in the Phase 8 surface per
    // 11-02b). Rust Task 2 holds the custom-consolidator proof of the
    // scope filter; TS closes only the report-count parity leg.
    for (const t of otherTurns) {
      await otherMoon.remember(t.text, "ctl");
      await otherPg.remember(t.text, "ctl");
    }
    const moonHits = await convMoon.recall(query);
    const pgHits = await convPg.recall(query);
    const divs = compareHits("multi_turn_conversation recall", moonHits, pgHits);

    const moonReport = await convMoon.consolidate();
    const pgReport = await convPg.consolidate();
    const moonN = (moonReport.promotions || []).length;
    const pgN = (pgReport.promotions || []).length;
    if (moonN !== pgN) {
      divs.push(`multi_turn consolidate promotions moon=${moonN} postgres=${pgN}`);
    }
    // Scope-isolation leakage flag — scan for other-user source prefix.
    for (const [report, label] of [
      [moonReport, "moon"],
      [pgReport, "postgres"],
    ] as [typeof moonReport, string][]) {
      for (const p of (report.promotions || []) as Array<{ episode?: { source?: string } }>) {
        const src = p?.episode?.source;
        if (typeof src === "string" && src.startsWith(`chat:${otherUserId}/`)) {
          divs.push(`${label} consolidate leaked other-user source: ${src}`);
        }
      }
    }
    expect(divs, `MultiTurnConversation parity: ${divs.join("; ")}`).toEqual([]);
  });

  test("slack_archive parity — channel filter on top of wide-recall", async () => {
    const both = await requireBothBackends();
    if (!both) return;
    const pair = await openPair(both.moon, both.pg);
    if (!pair) return;
    const fixture = loadFixture("slack_archive.json");
    const channels = fixture.channels as {
      id: string;
      messages: { user: string; text: string }[];
    }[];
    const query = fixture.query as string;
    const channelFilter = fixture.channel_filter as string;
    const { SlackArchive } = lunaris as {
      SlackArchive: new (h: unknown) => {
        ingest_channel: (ch: string, user: string, text: string) => Promise<string>;
        recall: (q: string) => Promise<Hit[]>;
        channel: (id: string) => { recall: (q: string) => Promise<Hit[]> };
      };
    };
    const slMoon = new SlackArchive(pair.moon);
    const slPg = new SlackArchive(pair.pg);
    for (const ch of channels) {
      for (const m of ch.messages) {
        await slMoon.ingest_channel(ch.id, m.user, m.text);
        await slPg.ingest_channel(ch.id, m.user, m.text);
      }
    }
    const divs: string[] = [];
    divs.push(
      ...compareHits("slack_archive wide", await slMoon.recall(query), await slPg.recall(query)),
    );
    divs.push(
      ...compareHits(
        `slack_archive channel=${channelFilter}`,
        await slMoon.channel(channelFilter).recall(query),
        await slPg.channel(channelFilter).recall(query),
      ),
    );
    expect(divs, `SlackArchive parity: ${divs.join("; ")}`).toEqual([]);
  });

  test("email_threading graph-off parity — default blueprint §5.2 path", async () => {
    const both = await requireBothBackends();
    if (!both) return;
    const pair = await openPair(both.moon, both.pg);
    if (!pair) return;
    const fixture = loadFixture("email_threading.json");
    const rootId = fixture.root_id as string;
    const messages = fixture.messages as { from: string; body: string }[];
    const query = fixture.query as string;
    const { EmailThreading } = lunaris as {
      EmailThreading: new (h: unknown) => {
        ingest: (rootId: string, from: string, body: string) => Promise<string>;
        thread: (rootId: string) => { recall: (q: string) => Promise<Hit[]> };
      };
    };
    // Blueprint §5.2 default — graph pipeline OFF on a fresh handle.
    const mh = pair.moon as { graphPipeline: { isEnabled: () => boolean } };
    const ph = pair.pg as { graphPipeline: { isEnabled: () => boolean } };
    expect(mh.graphPipeline.isEnabled(), "moon default must be graph-off").toBe(false);
    expect(ph.graphPipeline.isEnabled(), "postgres default must be graph-off").toBe(false);
    const emMoon = new EmailThreading(pair.moon);
    const emPg = new EmailThreading(pair.pg);
    for (const m of messages) {
      await emMoon.ingest(rootId, m.from, m.body);
      await emPg.ingest(rootId, m.from, m.body);
    }
    const moonHits = await emMoon.thread(rootId).recall(query);
    const pgHits = await emPg.thread(rootId).recall(query);
    expect(mh.graphPipeline.isEnabled(), "moon must remain graph-off after recall").toBe(false);
    expect(ph.graphPipeline.isEnabled(), "postgres must remain graph-off after recall").toBe(false);
    const divs = compareHits("email_threading graph-off", moonHits, pgHits);
    expect(divs, `EmailThreading parity: ${divs.join("; ")}`).toEqual([]);
  });

  test("email_threading graph-on opt-in — Gemma-gated toggle surface", async () => {
    // Risk row #2 — skip when Gemma weight env is not set.
    if (!process.env.LUNARIS_EXTRACT_GEMMA_PATH) {
      // eslint-disable-next-line no-console
      console.error(
        "conversational_parity: SKIP graph-on (LUNARIS_EXTRACT_GEMMA_PATH unset)",
      );
      return;
    }
    const both = await requireBothBackends();
    if (!both) return;
    const pair = await openPair(both.moon, both.pg);
    if (!pair) return;
    const { EmailThreading } = lunaris as {
      EmailThreading: new (h: unknown) => {
        with_graph_pipeline: (on: boolean) => { with_graph_pipeline: (on: boolean) => unknown };
      };
    };
    const mh = pair.moon as { graphPipeline: { isEnabled: () => boolean } };
    expect(mh.graphPipeline.isEnabled()).toBe(false);
    const em = new EmailThreading(pair.moon).with_graph_pipeline(true);
    expect(
      mh.graphPipeline.isEnabled(),
      "with_graph_pipeline(true) must enable the pipeline",
    ).toBe(true);
    em.with_graph_pipeline(false);
    expect(
      mh.graphPipeline.isEnabled(),
      "with_graph_pipeline(false) must disable the pipeline",
    ).toBe(false);
  });

  test("meeting_notes_memory parity — 10-note transcript Moon-vs-Postgres", async () => {
    const both = await requireBothBackends();
    if (!both) return;
    const pair = await openPair(both.moon, both.pg);
    if (!pair) return;
    const fixture = loadFixture("meeting_notes_memory.json");
    const notes = fixture.notes as { heading: string; body: string }[];
    const query = fixture.query as string;
    const { MeetingNotesMemory } = lunaris as {
      MeetingNotesMemory: new (h: unknown) => {
        note: (heading: string, body: string) => Promise<string>;
        recall: (q: string) => Promise<Hit[]>;
      };
    };
    const mnMoon = new MeetingNotesMemory(pair.moon);
    const mnPg = new MeetingNotesMemory(pair.pg);
    for (const n of notes) {
      await mnMoon.note(n.heading, n.body);
      await mnPg.note(n.heading, n.body);
    }
    const divs = compareHits(
      "meeting_notes_memory",
      await mnMoon.recall(query),
      await mnPg.recall(query),
    );
    expect(divs, `MeetingNotesMemory parity: ${divs.join("; ")}`).toEqual([]);
  });
});
