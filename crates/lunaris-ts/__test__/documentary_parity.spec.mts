// Phase 11 Plan 11-03 Task 3 — TypeScript documentary parity driver.
//
// TS-driver leg of the cross-language parity suite. Mirrors the 5
// Rust + Py scenarios from
// `crates/lunaris-recipes/tests/documentary_parity.rs`:
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
// Per-driver backend parity (Phase 8 CONTEXT — Moon rows == Postgres
// rows WITHIN the TypeScript run) is the correct interpretation of
// Phase 11 SC #5. Top-k SET equality (D-13 tie-bucket ordering
// accepted) is the cross-backend assertion inside this one Node
// process.
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

async function backendsOrNull(): Promise<{ moon: string; pg: string } | null> {
  const moon = await probeBackend("LUNARIS_MOON_URL");
  const pg = await probeBackend("LUNARIS_POSTGRES_URL");
  if (moon === null || pg === null) return null;
  return { moon, pg };
}

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
  corpus = corpus.with_graph_pipeline(false);
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
    await repo.ingest_commit(c.sha, ms, [[c.function_body_chunk, meta]]);
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
    const meta = { event_id: e.id, event_valid_time_unix_ms: ms };
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
    await hist.ingest_ticket(t.id, t.body);
  }
  for (const c of fx.chats) {
    await hist.ingest_chat(c.ticket_id, c.turn_idx, c.participant, c.msg);
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

  test("document_knowledge_base_parity_quickstart_rag", async () => {
    if (!wrappersPresent()) {
      console.error(
        "documentary_parity: SKIP (rebuild lunaris-ts with napi build to include 11-02b wrappers)",
      );
      return;
    }
    const backends = await backendsOrNull();
    if (backends === null) return;
    const g = loadGolden();
    const s = g.scenarios.document_knowledge_base_basic_rag as any;
    const moonHits = await runKbQuickstart(backends.moon, "moon", s.query, s.top_k);
    const pgHits = await runKbQuickstart(backends.pg, "pg", s.query, s.top_k);
    const moonSet = new Set(moonHits.map((h) => JSON.stringify(h)));
    const pgSet = new Set(pgHits.map((h) => JSON.stringify(h)));
    expect(moonSet).toEqual(pgSet);
    expect(moonHits.length).toBeGreaterThanOrEqual(s.expected_min_hits);
    const needles = s.expected_hit_body_contains_any as string[];
    expect(moonHits.some(([, body]) => needles.some((n) => body.includes(n)))).toBe(true);
  }, 60_000);

  test("research_paper_corpus_parity_graph_off_recall", async () => {
    if (!wrappersPresent()) return;
    const backends = await backendsOrNull();
    if (backends === null) return;
    const g = loadGolden();
    const s = g.scenarios.research_paper_corpus_graph_off as any;
    const moonHits = await runResearchPaper(backends.moon, "moon", s.query);
    const pgHits = await runResearchPaper(backends.pg, "pg", s.query);
    const moonSet = new Set(moonHits.map((h) => JSON.stringify(h)));
    const pgSet = new Set(pgHits.map((h) => JSON.stringify(h)));
    expect(moonSet).toEqual(pgSet);
    expect(moonHits.length).toBeGreaterThanOrEqual(s.expected_min_hits);
    const needles = s.expected_hit_body_contains_any as string[];
    expect(moonHits.some(([, body]) => needles.some((n) => body.includes(n)))).toBe(true);
  }, 60_000);

  test("code_repo_memory_parity_as_of_commit_50", async () => {
    if (!wrappersPresent()) return;
    const backends = await backendsOrNull();
    if (backends === null) return;
    const g = loadGolden();
    const s = g.scenarios.code_repo_memory_as_of_commit_50 as any;
    const moonTexts = await runCodeRepoAsOf(
      backends.moon, "moon", s.query, s.commit_index_0based,
    );
    const pgTexts = await runCodeRepoAsOf(
      backends.pg, "pg", s.query, s.commit_index_0based,
    );
    expect(new Set(moonTexts)).toEqual(new Set(pgTexts));
    expect(moonTexts.length).toBeGreaterThanOrEqual(s.expected_min_hits);
    const needle = s.expected_first_body_contains as string;
    expect(moonTexts.some((t) => t.includes(needle))).toBe(true);
    expect(pgTexts.some((t) => t.includes(needle))).toBe(true);
  }, 90_000);

  test("timeline_reconstruction_parity_between_10_and_15", async () => {
    if (!wrappersPresent()) return;
    const backends = await backendsOrNull();
    if (backends === null) return;
    const g = loadGolden();
    const s = g.scenarios.timeline_reconstruction_between_10_and_15 as any;
    const moonTexts = await runTimelineBetween(
      backends.moon, "moon", s.query,
      s.between_lo_rfc3339, s.between_hi_rfc3339,
    );
    const pgTexts = await runTimelineBetween(
      backends.pg, "pg", s.query,
      s.between_lo_rfc3339, s.between_hi_rfc3339,
    );
    expect(new Set(moonTexts)).toEqual(new Set(pgTexts));
    expect(moonTexts.length).toBe(s.expected_count);
    for (const needle of s.expected_event_ids_slice as string[]) {
      expect(moonTexts.some((t) => t.includes(needle))).toBe(true);
    }
  }, 60_000);

  test("customer_support_history_parity_refund_recall", async () => {
    if (!wrappersPresent()) return;
    const backends = await backendsOrNull();
    if (backends === null) return;
    const g = loadGolden();
    const s = g.scenarios.customer_support_refund_recall as any;
    for (const [label, url_] of [
      ["moon", backends.moon],
      ["pg", backends.pg],
    ] as const) {
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
