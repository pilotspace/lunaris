// Phase 11 Plan 11-03 Task 3 — TypeScript documentary parity driver.
//
// TS-driver leg of the cross-language parity suite. The 5 scenarios
// originally mirrored a Rust driver at
// `crates/lunaris-recipes/tests/documentary_parity.rs` — that file was
// DELETED in commit `03bf8bc` and only its fixtures survive, so the
// "three independent drivers (Rust / Python / TypeScript)" that
// `conformance-bindings.yml`'s header still advertises is now two.
// The scenarios:
//
//   1. DocumentKnowledgeBase — basic RAG, query "quickstart"
//   2. ResearchPaperCorpus — graph-off recall, query "reciprocal rank
//      fusion"
//   3. CodeRepoMemory — `TemporalQuery.as_of(commit_50_ts)` marquee
//      (Phase 11 SC #3)
//   4. TimelineReconstruction — `.between(2025-01-10, 2025-01-16)`
//      returns exactly 6 events (lower-inclusive / upper-exclusive per
//      Phase 9.1)
//   5. CustomerSupportHistory — "refund" recall preserves `ticket:` +
//      `chat:` source prefixes with unique `(source, id)` pairs
//
// WHAT THESE TESTS ASSERT (0.7.0 — CORRECTED, ship-plan W2.9)
// ------------------------------------------------------------
// Each scenario runs ONCE against a live Moon and asserts its rows
// against the committed golden. That is the whole contract now.
//
// Until W2.9 this file called itself a *backend* parity driver: every
// scenario ran twice — once on `LUNARIS_MOON_URL`, once on
// `LUNARIS_POSTGRES_URL` — and the headline assertion was top-k SET
// equality between the two. 0.7.0 deleted `lunaris-storage-postgres`;
// `lunaris.open` now rejects every scheme but `moon://` with
// `UnsupportedScheme`. The Postgres leg could not have run even against
// a live Postgres server, so `backendsOrNull()` — which demanded BOTH
// URLs — returned null on every invocation.
//
// THAT NULL WAS WORSE HERE THAN IN THE PYTHON DRIVER. Each test bailed
// with a bare `return`, which vitest reports as PASSED. Five tests were
// reporting green while asserting nothing — including under
// `conformance-bindings.yml`, which stands up a real Moon on 6391 and
// runs `npm test`. A test that cannot run must SKIP, never pass; every
// bail below now goes through `ctx.skip(reason)`.
//
// The cross-backend SET-equality assertion is GONE and is not
// recoverable — there is no second backend to compare against. What
// survives is per-driver golden conformance, which is what
// `conformance-bindings.yml`'s own header describes as the intent
// ("Each driver asserts its own rows against the committed golden
// reference"). Cross-LANGUAGE byte-identity was always out of scope.
//
// FOUR of the five scenarios now run for real. The fifth
// (`code_repo_memory_parity_as_of_commit_50`) is skipped against a
// NAMED product gap, not silently: Moon has no KV version chain, so a
// system-time `.as_of` pinned 18 months back — which is what the golden
// pins — is refused with `NotSupported`. See MOON_HISTORICAL_KV_READS
// below for the one-line unskip.
//
// NOTE ON THE NAMES: the `_parity_` infix in the five test names is now
// a misnomer. It is kept deliberately —
// `docs/book/src/cookbook/document-kb.md` and `research-and-code.md`
// cite these names, and those files are outside this change's scope.
// Rename both together.
//
// Import path: flat `import { ... } from "../index.mjs"` per Plan
// 11-02b's "Known limitations" — napi-rs 3.x's proc-macro registry
// surfaces every class as a top-level identifier; there is no
// `lunaris/documentary` subpath import in v0.1.1. The Plan 10-03
// commit `78fe5dc` landed the crate-root ESM re-exports for all five
// documentary classes plus the seven conversational classes, so the
// flat import below works against both the dev `maturin` + napi build
// output and the published `lunaris` npm wheel.

import { describe, expect, test } from "vitest";
import net from "node:net";
import fs from "node:fs";
import path from "node:path";
import url from "node:url";

const lunaris = await import("../index.mjs");

// Resolve paths. Layout:
//   crates/lunaris-ts/__test__/documentary_parity.spec.mts  (this file)
//   crates/lunaris-recipes/tests/fixtures/documentary/*.json
// Walk up 2 (__test__ → lunaris-ts → crates) then descend into lunaris-recipes.
const __dirname = path.dirname(url.fileURLToPath(import.meta.url));
const FIXTURES_ROOT = path.resolve(
  __dirname,
  "..",
  "..",
  "lunaris-recipes",
  "tests",
  "fixtures",
  "documentary",
);
const GOLDEN_PATH = path.join(FIXTURES_ROOT, "parity_golden.json");

interface Golden {
  schema_version: number;
  seed: string;
  scenarios: Record<string, Record<string, unknown>>;
}

function loadGolden(): Golden {
  if (!fs.existsSync(GOLDEN_PATH)) {
    throw new Error(
      `parity_golden.json missing at ${GOLDEN_PATH} — run from the lunaris repo checkout`,
    );
  }
  return JSON.parse(fs.readFileSync(GOLDEN_PATH, "utf-8")) as Golden;
}

function loadFixture<T>(name: string): T {
  const p = path.join(FIXTURES_ROOT, name);
  return JSON.parse(fs.readFileSync(p, "utf-8")) as T;
}

// ---------------------------------------------------------------------------
// Skip helpers (two-tier env + TCP probe per Plan 04-03 / 05-02).
// ---------------------------------------------------------------------------
function parseHostPort(u: string): { host: string; port: number } | null {
  // `moon://` only. The `postgres://` / `postgresql://` rows that used to
  // sit here died with `lunaris-storage-postgres` in 0.7.0 — `lunaris.open`
  // answers every other scheme with `UnsupportedScheme`, so parsing one
  // would only have produced a URL nothing could open.
  const schemes: [string, number][] = [["moon://", 6379]];
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
    console.error(`documentary_parity: SKIP ${envName} (env var unset)`);
    return null;
  }
  const parsed = parseHostPort(u);
  if (parsed === null) {
    console.error(`documentary_parity: SKIP ${envName} (unknown URL scheme)`);
    return null;
  }
  if (!(await reachable(parsed.host, parsed.port))) {
    console.error(
      `documentary_parity: SKIP ${envName} (TCP probe to ${parsed.host}:${parsed.port} failed)`,
    );
    return null;
  }
  return u;
}

// The one live backend. `moon://` is the only scheme `lunaris.open` accepts
// since 0.7.0, so there is nothing else to probe.
async function moonOrNull(): Promise<string | null> {
  return probeBackend("LUNARIS_MOON_URL");
}

/**
 * Bail out of a test as SKIPPED, never as passed.
 *
 * `ctx.skip(reason)` is the whole point of this helper: the bare `return`
 * it replaces made vitest report an un-run test as green. Anything that
 * cannot run must be visible as un-run.
 */
type SkippableCtx = { skip: (reason?: string) => void };

/**
 * Mirror of `lunaris_storage_moon::as_of::HISTORICAL_KV_READS`.
 *
 * Moon has no KV version chain, so `StoragePort::read_as_of` refuses any pin
 * older than `AS_OF_LIVE_WINDOW_MS` (1 h) with `StorageError::NotSupported`.
 * That makes the system-time `.as_of` scenario below unrunnable on the only
 * backend 0.7.0 ships. Flip this to `true` on the day the Rust constant flips,
 * and the scenario starts gating again.
 */
const MOON_HISTORICAL_KV_READS = false;

function wrappersPresent(): boolean {
  const l = lunaris as Record<string, unknown>;
  return (
    typeof l.DocumentKnowledgeBase === "function" &&
    typeof l.ResearchPaperCorpus === "function" &&
    typeof l.CodeRepoMemory === "function" &&
    typeof l.TimelineReconstruction === "function" &&
    typeof l.CustomerSupportHistory === "function"
  );
}

// Mirror of Rust `rfc3339_to_unix_ms`. Accepts `YYYY-MM-DDTHH:MM:SSZ`
// only (20 chars); rejects fractional seconds by shape check.
function rfc3339ToUnixMs(s: string): number {
  if (s.length !== 20 || s[s.length - 1] !== "Z" || s[10] !== "T") {
    throw new Error(`unsupported RFC3339 shape: ${s}`);
  }
  const y = Number.parseInt(s.slice(0, 4), 10);
  const mo = Number.parseInt(s.slice(5, 7), 10);
  const d = Number.parseInt(s.slice(8, 10), 10);
  const h = Number.parseInt(s.slice(11, 13), 10);
  const mi = Number.parseInt(s.slice(14, 16), 10);
  const se = Number.parseInt(s.slice(17, 19), 10);
  const yAdj = mo <= 2 ? y - 1 : y;
  // Euclidean division for correctness on negative years (algorithmic
  // equivalent of Rust's `div_euclid`).
  const era = yAdj >= 0 ? Math.floor(yAdj / 400) : -Math.floor((-yAdj + 399) / 400);
  const yoe = yAdj - era * 400;
  const doy = Math.floor((153 * (mo > 2 ? mo - 3 : mo + 9) + 2) / 5) + d - 1;
  const doe = yoe * 365 + Math.floor(yoe / 4) - Math.floor(yoe / 100) + doy;
  const daysFromCivil = era * 146097 + doe - 719468;
  const unixSeconds = daysFromCivil * 86400 + h * 3600 + mi * 60 + se;
  return unixSeconds * 1000;
}

type Hit = { id: unknown; source: string; text: string; [k: string]: unknown };

// ---------------------------------------------------------------------------
// Scenario runners.
// ---------------------------------------------------------------------------

async function runKbQuickstart(
  url_: string,
  backendLabel: string,
  query: string,
  topK: number,
): Promise<[string, string][]> {
  const l = lunaris as Record<string, any>;
  const mem = await l.open(url_);
  const prefix = `kb:docs/doc-11-03-ts/${backendLabel}/`;
  let kb = l.DocumentKnowledgeBase.new(mem, prefix);
  const docs = loadFixture<{ id: string; title: string; body: string }[]>(
    "document_knowledge_base_docs.json",
  );
  for (const d of docs) {
    const meta = { doc_id: d.id, title: d.title };
    await kb.ingest([[d.body, meta]]);
  }
  kb = kb.top(topK);
  const hits = (await kb.search(query)) as Hit[];
  return hits.map((h) => [h.source, h.text]);
}

async function runResearchPaper(
  url_: string,
  backendLabel: string,
  query: string,
): Promise<[string, string][]> {
  const l = lunaris as Record<string, any>;
  const mem = await l.open(url_);
  const prefix = `papers:doc-11-03-ts/${backendLabel}/`;
  let corpus = l.ResearchPaperCorpus.new(mem, prefix);
  corpus = corpus.withGraphPipeline(false);
  const papers = loadFixture<{ id: string; title: string; abstract: string }[]>(
    "research_paper_corpus_papers.json",
  );
  for (const p of papers) {
    const body = `${p.title}\n\n${p.abstract}`;
    const meta = { paper_id: p.id, title: p.title };
    await corpus.ingest([[body, meta]]);
  }
  const hits = (await corpus.search(query)) as Hit[];
  return hits.map((h) => [h.source, h.text]);
}

async function runCodeRepoAsOf(
  url_: string,
  backendLabel: string,
  query: string,
  commitIdx: number,
): Promise<string[]> {
  const l = lunaris as Record<string, any>;
  const mem = await l.open(url_);
  const prefix = `repo:doc-11-03-ts/${backendLabel}/`;
  const repo = l.CodeRepoMemory.new(mem, prefix);
  const commits = loadFixture<
    { sha: string; committer_date_rfc3339: string; function_body_chunk: string }[]
  >("code_repo_100_commits.json");
  const targetMs = rfc3339ToUnixMs(commits[commitIdx].committer_date_rfc3339);
  const asOf = { wall_ms: targetMs, counter: 0, node_id: 0 };
  for (const c of commits) {
    const ms = rfc3339ToUnixMs(c.committer_date_rfc3339);
    const meta = { function_name: "target" };
    await repo.ingestCommit(c.sha, ms, [[c.function_body_chunk, meta]]);
  }
  const hits = (await repo.recall(query, asOf)) as Hit[];
  return hits.map((h) => h.text);
}

async function runTimelineBetween(
  url_: string,
  backendLabel: string,
  query: string,
  loRfc: string,
  hiRfc: string,
): Promise<string[]> {
  const l = lunaris as Record<string, any>;
  const mem = await l.open(url_);
  const prefix = `timeline:doc-11-03-ts/${backendLabel}/`;
  const timeline = l.TimelineReconstruction.new(mem, prefix);
  const events = loadFixture<{ id: string; valid_time_rfc3339: string; text: string }[]>(
    "timeline_30_days.json",
  );
  for (const e of events) {
    const ms = rfc3339ToUnixMs(e.valid_time_rfc3339);
    const meta = { event_id: e.id, valid_time_unix_ms: ms };
    await timeline.ingest([[e.text, meta]]);
  }
  const lo = { wall_ms: rfc3339ToUnixMs(loRfc), counter: 0, node_id: 0 };
  const hi = { wall_ms: rfc3339ToUnixMs(hiRfc), counter: 0, node_id: 0 };
  const hits = (await timeline.between(query, lo, hi)) as Hit[];
  return hits.map((h) => h.text);
}

async function runCustomerSupportRefund(
  url_: string,
  query: string,
): Promise<[string, unknown][]> {
  const l = lunaris as Record<string, any>;
  const mem = await l.open(url_);
  const hist = l.CustomerSupportHistory.new(mem);
  const fx = loadFixture<{
    tickets: { id: string; body: string }[];
    chats: {
      ticket_id: string;
      turn_idx: number;
      participant: string;
      msg: string;
    }[];
  }>("customer_support_50_tickets.json");
  for (const t of fx.tickets) {
    await hist.ingestTicket(t.id, t.body);
  }
  for (const c of fx.chats) {
    await hist.ingestChat(c.ticket_id, c.turn_idx, c.participant, c.msg);
  }
  const hits = (await hist.recall(query)) as Hit[];
  return hits.map((h) => [h.source, h.id]);
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

describe("Plan 11-03 — documentary parity (TypeScript)", () => {
  test("parity_golden_json_loads_with_expected_scenarios", () => {
    // Offline golden-sanity — runs without backends.
    const g = loadGolden();
    expect(g.schema_version).toBe(1);
    expect(g.seed).toBe("lunaris-doc-parity-v1");
    for (const key of [
      "document_knowledge_base_basic_rag",
      "research_paper_corpus_graph_off",
      "code_repo_memory_as_of_commit_50",
      "timeline_reconstruction_between_10_and_15",
      "customer_support_refund_recall",
    ]) {
      expect(g.scenarios).toHaveProperty(key);
    }
  });

  test("document_knowledge_base_parity_quickstart_rag", async (ctx: SkippableCtx) => {
    if (!wrappersPresent()) {
      ctx.skip("rebuild lunaris-ts with napi build to include the 11-02b wrappers");
      return;
    }
    const moon = await moonOrNull();
    if (moon === null) {
      ctx.skip("LUNARIS_MOON_URL unset or unreachable");
      return;
    }
    const g = loadGolden();
    const s = g.scenarios.document_knowledge_base_basic_rag as any;
    const moonHits = await runKbQuickstart(moon, "moon", s.query, s.top_k);
    expect(moonHits.length).toBeGreaterThanOrEqual(s.expected_min_hits);
    const needles = s.expected_hit_body_contains_any as string[];
    expect(moonHits.some(([, body]) => needles.some((n) => body.includes(n)))).toBe(true);
  }, 60_000);

  test("research_paper_corpus_parity_graph_off_recall", async (ctx: SkippableCtx) => {
    if (!wrappersPresent()) {
      ctx.skip("rebuild lunaris-ts with napi build to include the 11-02b wrappers");
      return;
    }
    const moon = await moonOrNull();
    if (moon === null) {
      ctx.skip("LUNARIS_MOON_URL unset or unreachable");
      return;
    }
    const g = loadGolden();
    const s = g.scenarios.research_paper_corpus_graph_off as any;
    const moonHits = await runResearchPaper(moon, "moon", s.query);
    expect(moonHits.length).toBeGreaterThanOrEqual(s.expected_min_hits);
    const needles = s.expected_hit_body_contains_any as string[];
    expect(moonHits.some(([, body]) => needles.some((n) => body.includes(n)))).toBe(true);
  }, 60_000);

  test("code_repo_memory_parity_as_of_commit_50", async (ctx: SkippableCtx) => {
    // BLOCKED BY A PRODUCT GAP, not by the harness — and it is unblocked by
    // fixing Moon, not by editing this file.
    //
    // `CodeRepoMemory.recall(q, as_of)` is `TemporalQuery::<Documents>::as_of`,
    // which sets SYSTEM-time as_of on the RetrievalBuilder. `lunaris-retrieve`
    // hydrate.rs hands that straight to `StoragePort::read_as_of`, and
    // `lunaris-storage-moon` answers any pin older than `AS_OF_LIVE_WINDOW_MS`
    // (1 h) with `StorageError::NotSupported` — `HISTORICAL_KV_READS = false`
    // in `crates/lunaris-storage-moon/src/as_of.rs`, because Moon has no KV
    // version chain. This scenario's golden pins `as_of =
    // 2025-02-19T12:00:00Z`, roughly 18 months back, so the call throws rather
    // than returning rows. Moon is the only backend since 0.7.0, so there is
    // nowhere for it to pass.
    //
    // Skipped rather than deleted: the fixture and golden are still correct,
    // and this is the scenario that would prove bi-temporal time-travel
    // through the SDK the day the KV version chain lands. Skipped rather than
    // left running: a test known to fail is not a gate, it is a broken build.
    //
    // UNSKIP WHEN: `lunaris_storage_moon::as_of::HISTORICAL_KV_READS` is
    // true — flip MOON_HISTORICAL_KV_READS (defined above) to match it.
    if (!MOON_HISTORICAL_KV_READS) {
      ctx.skip(
        "0.7.0 product gap: Moon refuses historical KV reads " +
          "(HISTORICAL_KV_READS = false), so TemporalQuery.as_of throws " +
          "NotSupported. Unskip when HISTORICAL_KV_READS flips true.",
      );
      return;
    }
    if (!wrappersPresent()) {
      ctx.skip("rebuild lunaris-ts with napi build to include the 11-02b wrappers");
      return;
    }
    const moon = await moonOrNull();
    if (moon === null) {
      ctx.skip("LUNARIS_MOON_URL unset or unreachable");
      return;
    }
    const g = loadGolden();
    const s = g.scenarios.code_repo_memory_as_of_commit_50 as any;
    const moonTexts = await runCodeRepoAsOf(
      moon, "moon", s.query, s.commit_index_0based,
    );
    expect(moonTexts.length).toBeGreaterThanOrEqual(s.expected_min_hits);
    const needle = s.expected_first_body_contains as string;
    expect(moonTexts.some((t) => t.includes(needle))).toBe(true);
  }, 90_000);

  // W4.13 — the REFUSAL is the contract, and nothing asserted it.
  //
  // The scenario above is skipped against a named product gap, which is the
  // right call: a test known to fail is a broken build, not a gate. But
  // skipping it left the SDK-level time-travel story documented and untested in
  // BOTH directions — nothing checked that a historical `as_of` returns rows,
  // and nothing checked that it refuses either. An `as_of` that silently
  // returned an empty array, or that quietly answered with latest-state rows,
  // would have passed every test in this repo.
  //
  // That second failure mode is the dangerous one. Returning today's rows for a
  // pin 18 months back is a wrong answer that looks like a right one; the whole
  // point of `reject_historical_read` is that it refuses BEFORE issuing any
  // RESP command, so a rejected read cannot be confused with a transport
  // failure.
  //
  // The assertion keys on `moon_kv_as_of`, which
  // `crates/lunaris-storage-moon/src/as_of.rs` defines as the greppable machine
  // token for exactly this purpose, rather than on the prose that follows it.
  //
  // INVERT WHEN: `HISTORICAL_KV_READS` flips true — at which point this test
  // should assert rows come back and the scenario above should be unskipped.
  // Both are gated on the same mirrored constant so they cannot drift apart.
  test("historical as_of is refused, not silently empty", async (ctx: SkippableCtx) => {
    if (MOON_HISTORICAL_KV_READS) {
      ctx.skip(
        "HISTORICAL_KV_READS is true — the refusal this asserts no longer " +
          "applies. Unskip code_repo_memory_parity_as_of_commit_50 and delete " +
          "this test in the same commit.",
      );
      return;
    }
    if (!wrappersPresent()) {
      ctx.skip("rebuild lunaris-ts with napi build to include the 11-02b wrappers");
      return;
    }
    const moon = await moonOrNull();
    if (moon === null) {
      ctx.skip("LUNARIS_MOON_URL unset or unreachable");
      return;
    }
    const g = loadGolden();
    const s = g.scenarios.code_repo_memory_as_of_commit_50 as any;

    let caught: unknown = null;
    try {
      await runCodeRepoAsOf(moon, "moon", s.query, s.commit_index_0based);
    } catch (e) {
      caught = e;
    }
    expect(caught, "a historical as_of must throw, not return").not.toBeNull();
    expect(
      String(caught),
      "the throw must carry the moon_kv_as_of token — lunaris-server maps this " +
        "variant to a 501 — rather than being some other failure",
    ).toContain("moon_kv_as_of");
  }, 90_000);

  // F21 FIXED — the `test.fails` marker that used to sit here is gone, and the
  // assertions below are the ones it was parked in front of, unchanged.
  //
  // What it recorded: `TimelineReconstruction.ingest` forwards to
  // `DocumentCorpus::ingest`, which built the Episode with
  // `bt: BiTemporal::now(clock)` and `t_ref: None` and stored the caller's
  // valid-time as ordinary metadata. `.between(lo, hi)` renders into Moon's
  // `@valid_time:[lo hi]`, which matched the INGEST time, so a corpus of
  // January-2025 events ingested today had nothing in `[2025-01-10,
  // 2025-01-16)` and this body failed `expected 0 to be 6`.
  //
  // Three things had to change:
  //
  //   1. Core — the valid axis was not caller-settable ANYWHERE; no production
  //      path called `BiTemporal::at`. `Episode::ground_valid_axis` now moves
  //      it to `t_ref`, and every chunk inherits `episode.bt.valid.0`.
  //   2. Recipe — `DocumentCorpus` honours the reserved metadata key
  //      `valid_time_unix_ms`. Note the rename: this spec used to invent
  //      `event_valid_time_unix_ms`, and `DocumentCorpus` serves papers, docs
  //      and repos as well as timelines, so "event" was one caller's
  //      vocabulary imposed on the rest.
  //   3. The graph-OFF ingest path — the shipped default, and the one every
  //      DocumentCorpus recipe takes — never wrote a `valid_time_ms` field at
  //      all. Found only because this test kept failing after (1) and (2).
  //
  // HISTORY — F20, also fixed, used to sit in FRONT of F21 here. The TypeScript
  // SDK could not construct an `Hlc` at all: napi drops integer-ness above
  // u32::MAX, so `{ wall_ms: 1736467200000, ... }` arrived as a float and the
  // generated binding's `serde_json::from_value::<Hlc>` rejected it with
  // `VALIDATE: invalid type: floating point ...`. The bindings now lower
  // through `from_js`, which repairs the number shape first
  // (`lunaris_core::json_repair`). That stacking is the whole reason both
  // markers were chosen to FAIL when the defect goes away rather than to skip:
  // fixing F20 left `test.fails` green over F21, and only a marker that reds
  // on success surfaces the next layer. F20's own guard is
  // `__test__/hlc_bitemporal.spec.mts`, which asserts on the CALL rather than
  // on rows and therefore cannot be masked by anything here.
  test("timeline_reconstruction_parity_between_10_and_15", async (ctx: SkippableCtx) => {
    if (!wrappersPresent()) {
      ctx.skip("rebuild lunaris-ts with napi build to include the 11-02b wrappers");
      return;
    }
    const moon = await moonOrNull();
    if (moon === null) {
      ctx.skip("LUNARIS_MOON_URL unset or unreachable");
      return;
    }
    const g = loadGolden();
    const s = g.scenarios.timeline_reconstruction_between_10_and_15 as any;
    const moonTexts = await runTimelineBetween(
      moon, "moon", s.query,
      s.between_lo_rfc3339, s.between_hi_rfc3339,
    );
    // The sharpest assertion in the file: an EXACT count, which pins the
    // lower-inclusive / upper-exclusive `.between` boundary (Phase 9.1).
    expect(moonTexts.length).toBe(s.expected_count);
    for (const needle of s.expected_event_ids_slice as string[]) {
      expect(moonTexts.some((t) => t.includes(needle))).toBe(true);
    }
  }, 60_000);

  test("customer_support_history_parity_refund_recall", async (ctx: SkippableCtx) => {
    if (!wrappersPresent()) {
      ctx.skip("rebuild lunaris-ts with napi build to include the 11-02b wrappers");
      return;
    }
    const moon = await moonOrNull();
    if (moon === null) {
      ctx.skip("LUNARIS_MOON_URL unset or unreachable");
      return;
    }
    const g = loadGolden();
    const s = g.scenarios.customer_support_refund_recall as any;
    for (const [label, url_] of [["moon", moon]] as const) {
      const hits = await runCustomerSupportRefund(url_, s.query);
      const [ticketPrefix, chatPrefix] = s.expected_source_prefixes as [
        string, string,
      ];
      const tickets = hits.filter(([src]) => src.startsWith(ticketPrefix));
      const chats = hits.filter(([src]) => src.startsWith(chatPrefix));
      expect(tickets.length, `${label}: ticket-prefix hits`).toBeGreaterThanOrEqual(
        s.expected_min_ticket_hits,
      );
      expect(chats.length, `${label}: chat-prefix hits`).toBeGreaterThanOrEqual(
        s.expected_min_chat_hits,
      );
      if (s.expected_unique_source_ids as boolean) {
        const norm = (h: [string, unknown]): string => {
          const [src, id] = h;
          // id may surface as Buffer, Uint8Array, or a string depending
          // on the napi-rs serialiser path. Normalise to a hex-ish form
          // so the Set de-dupe is stable.
          if (Buffer.isBuffer(id)) return `${src}|${id.toString("hex")}`;
          if (id instanceof Uint8Array)
            return `${src}|${Buffer.from(id).toString("hex")}`;
          return `${src}|${String(id)}`;
        };
        const set = new Set(hits.map(norm));
        expect(set.size, `${label}: duplicate (source, id) pairs`).toBe(hits.length);
      }
    }
  }, 90_000);
});
