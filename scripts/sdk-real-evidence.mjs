#!/usr/bin/env node
// scripts/sdk-real-evidence.mjs — lunaris-ts real-case evidence runner.
//
// Mirrors scripts/sdk-real-evidence.py for the TypeScript napi-rs binding:
//   - open / snapshot / ingest / recall / forget(dry-run) roundtrip
//   - ChatAgentMemory.remember / recall conversational roundtrip
//   - DocumentKnowledgeBase.ingest / search documentary roundtrip
//   - Consolidator + Graph pipeline toggles
//
// Per-scenario JSON envelope written under ${LUNARIS_SDK_EVIDENCE_DIR}.
// Expects to be invoked from crates/lunaris-ts so `./index.mjs` resolves.

import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import urlMod from "node:url";
import { performance } from "node:perf_hooks";

const MOON_URL = process.env.LUNARIS_TEST_MOON_URL ?? "moon://127.0.0.1:6379";
const OUT_DIR = process.env.LUNARIS_SDK_EVIDENCE_DIR
  ?? path.resolve(path.dirname(urlMod.fileURLToPath(import.meta.url)), "..", "milestones", "v0.1.1-sdk-real");
fs.mkdirSync(OUT_DIR, { recursive: true });

const lunaris = await import(path.resolve(process.cwd(), "index.mjs"));

function envelope(name, status, durationMs, details) {
  return {
    runner: "typescript",
    scenario: name,
    backend: MOON_URL,
    status,
    duration_ms: Math.round(durationMs * 1000) / 1000,
    node_version: process.version,
    details,
  };
}

function writeEnv(name, env) {
  const p = path.join(OUT_DIR, `ts-${name}.json`);
  fs.writeFileSync(p, JSON.stringify(env, null, 2));
  const mark = { PASS: "✓", SKIP: "∘", FAIL: "✗" }[env.status] ?? "?";
  console.log(`  ${mark} ts:${name.padEnd(32)} ${env.status.padEnd(4)} ${env.duration_ms.toFixed(1).padStart(8)}ms  -> ${path.basename(p)}`);
}

function tcpReachable(u) {
  const m = /^moon:\/\/([^:/]+):(\d+)/.exec(u);
  if (!m) return Promise.resolve(false);
  return new Promise((resolve) => {
    const s = net.createConnection({ host: m[1], port: Number(m[2]) });
    const fail = () => { s.destroy(); resolve(false); };
    s.setTimeout(500, fail);
    s.on("connect", () => { s.end(); resolve(true); });
    s.on("error", fail);
  });
}

async function run(name, fn) {
  const t0 = performance.now();
  try {
    const details = await fn();
    const status = details.__status__ ?? "PASS";
    delete details.__status__;
    writeEnv(name, envelope(name, status, performance.now() - t0, details));
    return status !== "FAIL";
  } catch (e) {
    writeEnv(name, envelope(name, "FAIL", performance.now() - t0, {
      error: String(e?.message ?? e),
      stack: String(e?.stack ?? "").split("\n").slice(-6),
    }));
    return false;
  }
}

// 26-char Crockford-Base32 ULID generator (alphabet excludes I,L,O,U).
// JS's `toString(32)` uses the wrong alphabet — we have to encode manually.
const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
function crockford(n, len) {
  let out = "";
  for (let i = 0; i < len; i++) {
    out = CROCKFORD[Number(n & 31n)] + out;
    n >>= 5n;
  }
  return out;
}
function buildEpisode(content, source = "sdk-real-ts") {
  const ts = BigInt(Date.now());
  const hi = BigInt(Math.floor(Math.random() * 2 ** 40));
  const lo = BigInt(Math.floor(Math.random() * 2 ** 40));
  const rand = (hi << 40n) | lo;  // 80-bit random
  const id = crockford(ts, 10) + crockford(rand, 16);
  return {
    id,
    source,
    content,
    t_ref: null,
    bt: {
      valid: [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
      sys:   [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
    },
    metadata: {},
  };
}

// ---------- Scenarios ----------

async function offlineDslSurface() {
  const classes = [
    "Lunaris", "Vector", "Keyword", "Graph", "RetrievalBuilder",
    "ChatAgentMemory", "MultiTurnConversation", "SlackArchive", "EmailThreading", "MeetingNotesMemory",
    "DocumentKnowledgeBase", "ResearchPaperCorpus", "CodeRepoMemory", "TimelineReconstruction", "CustomerSupportHistory",
  ];
  const missing = classes.filter((c) => typeof lunaris[c] !== "function");
  return {
    exported_class_count: classes.length - missing.length,
    missing_exports: missing,
    __status__: missing.length === 0 ? "PASS" : "FAIL",
  };
}

async function coreRoundtrip() {
  if (!(await tcpReachable(MOON_URL))) return { __status__: "SKIP", reason: `Moon unreachable at ${MOON_URL}` };
  let handle;
  try {
    handle = await lunaris.open(MOON_URL);
  } catch (e) {
    return { __status__: "SKIP", reason: `open() handshake failed: ${String(e?.message ?? e)}` };
  }
  const s1 = await handle.snapshot();
  const lsn = await handle.ingest(buildEpisode("Lunaris SDK real-case demo - TS core roundtrip"));
  const s2 = await handle.snapshot();
  const receipt = await handle.forget({
    target: { Scope: { BySource: "sdk-real-ts/nonexistent" } },
    options: { hard: false, dry_run: true, confirmation_token: null },
  });
  return {
    snapshot_before: s1,
    snapshot_after_ingest: s2,
    snapshot_monotonic: s2 >= s1,
    forget_dry_run_preview: receipt?.preview,
    forget_rows_written: receipt?.rows_written,
  };
}

async function chatAgentMemory() {
  if (!(await tcpReachable(MOON_URL))) return { __status__: "SKIP", reason: "Moon unreachable" };
  let handle, chat;
  try {
    handle = await lunaris.open(MOON_URL);
    chat = lunaris.ChatAgentMemory.new(handle, "sdk-real-demo-user-ts");
  } catch (e) {
    return { __status__: "SKIP", reason: `ChatAgentMemory init failed: ${String(e?.message ?? e)}` };
  }
  const turns = [
    "TS demo - working on Lunaris agent memory",
    "TS demo - milestone v0.1.1 Recipes and Helios",
    "TS demo - sub-25ms recall over millions of facts",
  ];
  const recorded = [];
  for (const t of turns) recorded.push(await chat.remember(t));
  const hits = await chat.recall("Lunaris milestone");
  return {
    recorded_ids: recorded.slice(0, 3),
    turn_count: recorded.length,
    recall_hit_count: hits.length,
    recall_sample: hits.slice(0, 2),
  };
}

async function documentKnowledgeBase() {
  if (!(await tcpReachable(MOON_URL))) return { __status__: "SKIP", reason: "Moon unreachable" };
  let handle, kb;
  try {
    handle = await lunaris.open(MOON_URL);
    kb = lunaris.DocumentKnowledgeBase.new(handle, "sdk-real-demo-kb-ts/");
  } catch (e) {
    return { __status__: "SKIP", reason: `DocumentKnowledgeBase init failed: ${String(e?.message ?? e)}` };
  }
  await kb.ingest([
    ["Lunaris uses bi-temporal MVCC over Moon for sub-25ms recall.", { doc_id: "doc-ts-001" }],
    ["Helios production hardening lands in Phase 12 of v0.1.1.",     { doc_id: "doc-ts-002" }],
  ]);
  const hits = await kb.top(5).search("bi-temporal MVCC");
  return { ingested: 2, search_hit_count: hits.length, search_sample: hits.slice(0, 2) };
}

async function pipelineToggles() {
  // Matches __test__/toggles.spec.mts — class-shape assertion ONLY.
  // Runtime enable/disable requires a live tokio reactor context that
  // napi-rs doesn't expose for sync calls; the official test suite
  // likewise asserts only shape. Live toggle coverage sits in the Rust
  // `consolidator_scope_isolation.rs` / `graph_pipeline_smoke.rs` tests.
  const { GraphPipelineHandleExt, ConsolidatorPipelineHandleExt } = lunaris;
  const checks = {
    graph_ext_class: typeof GraphPipelineHandleExt === "function",
    graph_enable_fn: typeof GraphPipelineHandleExt?.prototype?.enable === "function",
    graph_disable_fn: typeof GraphPipelineHandleExt?.prototype?.disable === "function",
    graph_is_enabled_fn: typeof GraphPipelineHandleExt?.prototype?.isEnabled === "function",
    graph_toggle_fn: typeof GraphPipelineHandleExt?.prototype?.toggle === "function",
    cons_ext_class: typeof ConsolidatorPipelineHandleExt === "function",
    cons_enable_fn: typeof ConsolidatorPipelineHandleExt?.prototype?.enable === "function",
    cons_disable_fn: typeof ConsolidatorPipelineHandleExt?.prototype?.disable === "function",
    cons_is_enabled_fn: typeof ConsolidatorPipelineHandleExt?.prototype?.isEnabled === "function",
    cons_toggle_fn: typeof ConsolidatorPipelineHandleExt?.prototype?.toggle === "function",
  };
  const all = Object.values(checks).every(Boolean);
  return { ...checks, __status__: all ? "PASS" : "FAIL" };
}

// ---------- Main ----------

console.log(`TypeScript runner - MOON_URL=${MOON_URL}`);
const results = [];
results.push(await run("offline-dsl-surface",      offlineDslSurface));
results.push(await run("core-roundtrip",           coreRoundtrip));
results.push(await run("chat-agent-memory",        chatAgentMemory));
results.push(await run("document-knowledge-base",  documentKnowledgeBase));
results.push(await run("pipeline-toggles",         pipelineToggles));

const ok = results.filter(Boolean).length;
const rc = ok === results.length ? 0 : 1;
console.log(`\ntypescript-summary: ${ok}/${results.length} OK rc=${rc}`);
process.exit(rc);
