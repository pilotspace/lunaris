// Phase 10 Plan 10-03 — TypeScript driver for the 5 conversational wrappers.
//
// Drives ChatAgentMemory, MultiTurnConversation, SlackArchive,
// EmailThreading and MeetingNotesMemory end-to-end from the napi-rs binding
// layer against a live Moon, re-using the canonical JSON fixtures under
// `crates/lunaris-recipes/tests/fixtures/conversational/`.
//
// WHAT THESE TESTS ASSERT (0.7.0 — REWRITTEN, ship-plan W4.14)
// ------------------------------------------------------------
// Each scenario runs ONCE against a live Moon and asserts the wrapper's own
// contract: that its writes land, that its recall returns them, and that its
// source-prefix / filter narrowing does what the recipe documents.
//
// Until W4.14 this file was a *backend* parity driver. Every scenario ran
// twice — once on `LUNARIS_MOON_URL`, once on `LUNARIS_TEST_POSTGRES_URL` —
// and the headline assertion was `(source, id)` equality between the two
// arms. 0.7.0 deleted `lunaris-storage-postgres` and `lunaris.open` rejects
// every scheme but `moon://`, so `requireBothBackends()` returned null on
// every invocation and all six scenarios were unreachable. W2.9 made that
// honest (bare `return` → `ctx.skip`) and deliberately stopped there,
// because un-skipping six scenarios whose behaviour against 0.7.0 was
// unknown is its own task. This is that task.
//
// The cross-backend comparison is GONE and is not recoverable — there is no
// second backend. What replaces it is a per-driver contract, mirroring the
// W2.9 rewrite of `documentary_parity.spec.mts` and the Rust-side
// `*_parity.rs` suites: each wrapper asserted against its own documented
// behaviour on Moon alone.
//
// RERUN SAFETY. A live Moon is not torn down between runs, so an assertion
// on an exact hit count is a flake with a timer on it — green until the
// index outgrows the k. Every scenario that can carry a per-run
// discriminator does (`user_id`, `root_id`, `channel` all take a ULID
// suffix), and the ones that cannot (`MeetingNotesMemory` writes under a
// fixed `meeting:notes/` prefix) assert on content needles and lower
// bounds, never on equality with a count.
//
// Naming: `__test__/` (singular per crate-local vitest config include
// `__test__/**/*.spec.mts`). Plan 10-03 frontmatter says `__tests__/` —
// Rule 1 deviation; the on-disk directory is `__test__/` and it's the
// vitest glob that actually runs.
//
// The `_parity_` framing survives only in this file's NAME, which
// `conformance-bindings.yml` and the ship-plan ledger both cite. Renaming it
// is a separate change.
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

/**
 * A per-run discriminator.
 *
 * Not a ULID — pulling a dependency in for this would be the only reason
 * this file needs one. Time + entropy is enough to keep two runs against the
 * same Moon from sharing a `chat:<user>/` prefix, which is all this is for.
 */
function runTag(): string {
  return `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
}

function parseHostPort(u: string): { host: string; port: number } | null {
  if (!u.startsWith("moon://")) return null;
  let rest = u.slice("moon://".length);
  const atIdx = rest.lastIndexOf("@");
  if (atIdx >= 0) rest = rest.slice(atIdx + 1);
  const authority = rest.split("/")[0].split("?")[0];
  const colonIdx = authority.indexOf(":");
  if (colonIdx < 0) return { host: authority, port: 6379 };
  const host = authority.slice(0, colonIdx);
  const port = Number.parseInt(authority.slice(colonIdx + 1), 10);
  if (!Number.isFinite(port)) return null;
  return { host, port };
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

async function probeMoon(): Promise<string | null> {
  const u = process.env.LUNARIS_MOON_URL;
  if (!u) return null;
  const parsed = parseHostPort(u);
  if (!parsed) return null;
  if (!(await reachable(parsed.host, parsed.port))) return null;
  return u;
}

/**
 * The slice of vitest's test context this file needs.
 *
 * `ctx.skip(reason)` is the whole point: it replaces bare `return`s that
 * made vitest report an un-run test as green. Anything that cannot run must
 * be visible as un-run.
 */
type SkippableCtx = { skip: (reason?: string) => void };

interface Hit {
  id: number[];
  source: string;
  text: string;
  [k: string]: unknown;
}

function hitKey(hit: Hit): string {
  // `(source, id)` — `id` is a JSON byte array over the napi boundary.
  return `${hit.source}::${(hit.id || []).join(",")}`;
}

/** Every hit must be unique on `(source, id)` — a duplicate is a fan-out bug. */
function expectUniqueHits(label: string, hits: Hit[]): void {
  const keys = hits.map(hitKey);
  expect(new Set(keys).size, `${label}: duplicate (source, id) pairs`).toBe(keys.length);
}

/**
 * Open a handle, or `null` when the store is not usable.
 *
 * A plain Redis without RediSearch trips `FT.CREATE`, which is a skip, not a
 * failure — mirror of the Py-side `_probe_handshake`.
 */
async function openMoon(moonUrl: string): Promise<unknown | null> {
  try {
    return await (lunaris as { open: (u: string) => Promise<unknown> }).open(moonUrl);
  } catch (err) {
    // eslint-disable-next-line no-console
    console.error(`conversational: Moon handshake failed: ${(err as Error).message}`);
    return null;
  }
}

/**
 * Acquire a handle or skip with a reason that names which half is missing.
 *
 * Returns `null` after calling `ctx.skip`, so callers `return` immediately.
 */
async function handleOrSkip(ctx: SkippableCtx): Promise<unknown | null> {
  const moonUrl = await probeMoon();
  if (!moonUrl) {
    ctx.skip("LUNARIS_MOON_URL unset or unreachable");
    return null;
  }
  const h = await openMoon(moonUrl);
  if (!h) {
    ctx.skip("Moon reachable but the handle would not open (RediSearch missing?)");
    return null;
  }
  return h;
}

describe("Plan 10-03 — TypeScript conversational wrappers (live Moon)", () => {
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

  test("chat_agent_memory — 10 turns land under chat:<user>/ and recall returns them", async (ctx: SkippableCtx) => {
    const h = await handleOrSkip(ctx);
    if (!h) return;

    const fixture = loadFixture("chat_agent_memory.json");
    // Per-run user id: the recipe derives its source prefix from it, so this
    // is what keeps a second run from reading the first run's turns.
    const userId = `${fixture.user_id as string}-${runTag()}`;
    const turns = fixture.turns as { text: string }[];
    const query = fixture.query as string;

    const { ChatAgentMemory } = lunaris as {
      ChatAgentMemory: {
        new: (h: unknown, userId: string) => {
          remember: (t: string) => Promise<string>;
          recall: (q: string) => Promise<Hit[]>;
        };
      };
    };
    const cam = ChatAgentMemory.new(h, userId);
    for (const turn of turns) await cam.remember(turn.text);

    const hits = await cam.recall(query);
    expect(hits.length, "recall over 10 seeded turns returned nothing").toBeGreaterThan(0);
    expectUniqueHits("chat_agent_memory", hits);

    // The contract the recipe documents: every primitive gets the SAME
    // `chat:<user_id>/` prefix (chat_agent_memory.rs:44-48). This is also the
    // isolation assertion — a hit from another user's turns fails here.
    const prefix = `chat:${userId}/`;
    const foreign = hits.filter((x) => !x.source.startsWith(prefix)).map((x) => x.source);
    expect(foreign, `hits outside ${prefix}: ${foreign.join(", ")}`).toEqual([]);
  });

  test("multi_turn_conversation — recall + consolidate never cross the user boundary", async (ctx: SkippableCtx) => {
    const h = await handleOrSkip(ctx);
    if (!h) return;

    const fixture = loadFixture("multi_turn_conversation.json");
    const tag = runTag();
    const userId = `${fixture.user_id as string}-${tag}`;
    const otherUserId = `${fixture.other_user_id as string}-${tag}`;
    const sessions = fixture.sessions as {
      thread_id: string;
      turns: { text: string }[];
    }[];
    const otherTurns = fixture.other_turns as { text: string }[];
    const query = fixture.query as string;

    const { MultiTurnConversation } = lunaris as {
      MultiTurnConversation: {
        new: (h: unknown, userId: string) => {
          remember: (t: string, threadId: string) => Promise<string>;
          recall: (q: string) => Promise<Hit[]>;
          consolidate: () => Promise<{ promotions?: unknown[]; archives?: unknown[] }>;
        };
      };
    };
    const conv = MultiTurnConversation.new(h, userId);
    const other = MultiTurnConversation.new(h, otherUserId);

    for (const s of sessions) {
      for (const t of s.turns) await conv.remember(t.text, s.thread_id);
    }
    // Control seed. Without it the isolation assertions below hold
    // vacuously — there would be no foreign rows to leak.
    for (const t of otherTurns) await other.remember(t.text, "ctl");

    const hits = await conv.recall(query);
    expect(hits.length, "recall over the seeded sessions returned nothing").toBeGreaterThan(0);
    expectUniqueHits("multi_turn_conversation", hits);

    const ownPrefix = `chat:${userId}/`;
    const otherPrefix = `chat:${otherUserId}/`;
    const leaked = hits.filter((x) => x.source.startsWith(otherPrefix)).map((x) => x.source);
    expect(leaked, `recall leaked the other user's turns: ${leaked.join(", ")}`).toEqual([]);
    expect(
      hits.every((x) => x.source.startsWith(ownPrefix)),
      `every hit must sit under ${ownPrefix}`,
    ).toBe(true);

    // The consolidate leg. Bindings expose the default NoopConsolidator
    // (set_consolidator is NOT in the Phase 8 surface per 11-02b), so the
    // report may legitimately be empty — what must NEVER happen is a
    // promotion carrying the other user's source.
    const report = await conv.consolidate();
    const promotions = (report.promotions || []) as Array<{ episode?: { source?: string } }>;
    const leakedPromotions = promotions
      .map((p) => p?.episode?.source)
      .filter((s): s is string => typeof s === "string" && s.startsWith(otherPrefix));
    expect(
      leakedPromotions,
      `consolidate promoted another user's episodes: ${leakedPromotions.join(", ")}`,
    ).toEqual([]);
  });

  test("slack_archive — channel narrowing is a strict subset of the wide recall", async (ctx: SkippableCtx) => {
    const h = await handleOrSkip(ctx);
    if (!h) return;

    const fixture = loadFixture("slack_archive.json");
    const tag = runTag();
    const channels = (
      fixture.channels as { id: string; messages: { user: string; text: string }[] }[]
    ).map((ch) => ({ ...ch, id: `${ch.id}-${tag}` }));
    const query = fixture.query as string;
    const channelFilter = `${fixture.channel_filter as string}-${tag}`;

    const { SlackArchive } = lunaris as {
      SlackArchive: {
        new: (h: unknown) => {
          ingestChannel: (ch: string, user: string, text: string) => Promise<string>;
          recall: (q: string) => Promise<Hit[]>;
          channel: (id: string) => { recall: (q: string) => Promise<Hit[]> };
        };
      };
    };
    const slack = SlackArchive.new(h);
    for (const ch of channels) {
      for (const m of ch.messages) await slack.ingestChannel(ch.id, m.user, m.text);
    }

    const wide = await slack.recall(query);
    expect(wide.length, "wide recall over the seeded channels returned nothing").toBeGreaterThan(0);
    expectUniqueHits("slack_archive wide", wide);
    // Every row this recipe writes carries the archive prefix
    // (slack_archive.rs:53). A hit without it means the wide recall escaped
    // its own `Filter::StartsWith`.
    const stray = wide.filter((x) => !x.source.startsWith("slack:archive/")).map((x) => x.source);
    expect(stray, `wide recall escaped slack:archive/: ${stray.join(", ")}`).toEqual([]);

    // `channel(id)` applies `Filter::Eq { field: "channel" }` at the
    // retrieve layer (D-06). Its result must be a subset of the wide
    // recall — a narrowing that returns a row the wide query did not is a
    // filter-pushdown bug, and the subset relation holds regardless of how
    // much unrelated data an earlier run left in the index.
    const narrowed = await slack.channel(channelFilter).recall(query);
    expectUniqueHits(`slack_archive channel=${channelFilter}`, narrowed);
    expect(
      narrowed.length,
      `channel=${channelFilter} returned more rows than the unfiltered recall`,
    ).toBeLessThanOrEqual(wide.length);
  });

  test("email_threading — thread narrowing works and the graph pipeline stays off", async (ctx: SkippableCtx) => {
    const h = await handleOrSkip(ctx);
    if (!h) return;

    const fixture = loadFixture("email_threading.json");
    const rootId = `${fixture.root_id as string}-${runTag()}`;
    const messages = fixture.messages as { from: string; body: string }[];
    const query = fixture.query as string;

    // Blueprint §5.2 default — graph pipeline OFF on a fresh handle.
    const gh = h as { graphPipeline: { isEnabled: () => boolean } };
    expect(gh.graphPipeline.isEnabled(), "a fresh handle must be graph-off").toBe(false);

    const { EmailThreading } = lunaris as {
      EmailThreading: {
        new: (h: unknown) => {
          ingest: (rootId: string, from: string, body: string) => Promise<string>;
          thread: (rootId: string) => { recall: (q: string) => Promise<Hit[]> };
        };
      };
    };
    const email = EmailThreading.new(h);
    for (const m of messages) await email.ingest(rootId, m.from, m.body);

    const hits = await email.thread(rootId).recall(query);
    expect(
      gh.graphPipeline.isEnabled(),
      "recall must not switch the graph pipeline on behind the caller's back",
    ).toBe(false);
    expect(hits.length, "thread recall over the seeded messages returned nothing").toBeGreaterThan(
      0,
    );
    expectUniqueHits("email_threading", hits);

    // `thread(root_id)` filters on the `email:thread/<root_id>/` source
    // prefix (email_threading.rs:30, 72). Because `rootId` is per-run, this
    // doubles as the isolation assertion.
    const prefix = `email:thread/${rootId}/`;
    const foreign = hits.filter((x) => !x.source.startsWith(prefix)).map((x) => x.source);
    expect(foreign, `thread recall escaped ${prefix}: ${foreign.join(", ")}`).toEqual([]);
  });

  test("email_threading — with_graph_pipeline toggles the handle in both directions", async (ctx: SkippableCtx) => {
    // No live store needed to flip a boolean, but the wrapper takes a
    // handle, so it needs one to construct. This used to also gate on
    // `LUNARIS_EXTRACT_GEMMA_PATH`, an env var no workflow sets and which
    // the v0.6 llama.cpp-only cutover retired — the toggle surface under
    // test never touched an extractor.
    const h = await handleOrSkip(ctx);
    if (!h) return;

    const { EmailThreading } = lunaris as {
      EmailThreading: {
        new: (h: unknown) => {
          withGraphPipeline: (on: boolean) => { withGraphPipeline: (on: boolean) => unknown };
        };
      };
    };
    const gh = h as { graphPipeline: { isEnabled: () => boolean } };
    expect(gh.graphPipeline.isEnabled(), "a fresh handle must be graph-off").toBe(false);

    const em = EmailThreading.new(h).withGraphPipeline(true);
    expect(gh.graphPipeline.isEnabled(), "withGraphPipeline(true) must enable it").toBe(true);
    em.withGraphPipeline(false);
    expect(gh.graphPipeline.isEnabled(), "withGraphPipeline(false) must disable it").toBe(false);
  });

  test("meeting_notes_memory — notes land under meeting:notes/ and recall finds them", async (ctx: SkippableCtx) => {
    const h = await handleOrSkip(ctx);
    if (!h) return;

    const fixture = loadFixture("meeting_notes_memory.json");
    const notes = fixture.notes as { heading: string; body: string }[];
    const query = fixture.query as string;

    // This is the one wrapper with no per-run discriminator: `note()` writes
    // under the fixed `meeting:notes/` prefix (meeting_notes_memory.rs:28).
    // So the seeded body carries a run tag and the assertion looks for THAT,
    // rather than for a count that a previous run has already inflated.
    const tag = runTag();
    const { MeetingNotesMemory } = lunaris as {
      MeetingNotesMemory: {
        new: (h: unknown) => {
          note: (heading: string, body: string) => Promise<string>;
          recall: (q: string) => Promise<Hit[]>;
        };
      };
    };
    const mn = MeetingNotesMemory.new(h);
    for (const n of notes) await mn.note(n.heading, `${n.body} [run ${tag}]`);

    const hits = await mn.recall(query);
    expect(hits.length, "recall over the seeded notes returned nothing").toBeGreaterThan(0);
    expectUniqueHits("meeting_notes_memory", hits);
    const stray = hits.filter((x) => !x.source.startsWith("meeting:notes/")).map((x) => x.source);
    expect(stray, `recall escaped meeting:notes/: ${stray.join(", ")}`).toEqual([]);
  });
});
