# Lunaris

[![CI](https://github.com/pilotspace/lunaris/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/pilotspace/lunaris/actions/workflows/ci.yml)
[![recall-ratchet](https://github.com/pilotspace/lunaris/actions/workflows/recall-ratchet.yml/badge.svg?branch=main)](https://github.com/pilotspace/lunaris/actions/workflows/recall-ratchet.yml)
[![crates.io](https://img.shields.io/crates/v/lunaris-memory.svg?label=crates.io%20lunaris-memory)](https://crates.io/crates/lunaris-memory)
[![docs.rs](https://img.shields.io/docsrs/lunaris-memory?label=docs.rs)](https://docs.rs/lunaris-memory)
[![PyPI](https://img.shields.io/pypi/v/lunaris.svg?label=pypi%20lunaris)](https://pypi.org/project/lunaris/)
[![npm](https://img.shields.io/npm/v/%40pilotspace%2Flunaris.svg?label=npm%20%40pilotspace%2Flunaris)](https://www.npmjs.com/package/@pilotspace/lunaris)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue.svg)](CONTRIBUTING.md#prerequisites)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Sub-25 ms recall at 100,000 documents per scope — measured, not
projected — with provable atomicity and a graph that's opt-in.**

<sub>p50 19.2–22.4 ms / p99 23.4–24.4 ms, engine-side, single-shard Moon
v0.8.5 on an Apple M4 Pro — [full envelope and method](docs/operations/capacity.md).
Beyond 100k the contract is unvalidated; do not read "millions".</sub>

A production-grade agent-memory engine in Rust, with first-class Python
and TypeScript SDKs and a zero-Rust MCP server for coding agents. Raw
observations in; structured, bi-temporal facts out. Backed by **Moon**, the
high-performance Redis-compatible substrate — and, as of 0.7.0, only Moon.
The Postgres and SQLite backends were removed; see
[0.6 → 0.7](docs/migration/0.6-to-0.7.md) if you are on one.

![Lunaris layered architecture](https://raw.githubusercontent.com/pilotspace/lunaris/main/docs/book/src/images/architecture/lunaris-layers.png)

> **Documentation:** the full guide lives in the
> **[Lunaris Book](https://pilotspace.github.io/lunaris/)**, or run
> `mdbook serve docs/book` to read it locally. **First time here?** [`docs/POSITIONING.md`](docs/POSITIONING.md) is
> the one-page pitch + honest "use a different tool when…" criteria.
> **How it works:** [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — the
> layered design and the Moon advantage map, every claim proof-anchored.

## Pick your path

| You are… | Do this | Time |
|---|---|---|
| **Giving your AI agent memory** (Claude Code, Codex) | [Install the MCP server](#1-give-your-ai-agent-memory-mcp) — no Rust toolchain needed | 2 min |
| **Building an app** in Python / TypeScript / Rust | [Install an SDK](#2-build-with-an-sdk) | 5 min |
| **Evaluating** against Mem0 / Zep / Cognee | Read [`POSITIONING.md`](docs/POSITIONING.md), then the [migration doc](#coming-from-another-agent-memory-tool) for your tool | 10 min |

---

## 1. Give your AI agent memory (MCP)

The MCP server gives any MCP-capable agent **16 memory tools** — eight
durable-memory tools (`memory.ingest`, `memory.recall`, `memory.forget`,
`memory.list_scopes`, `memory.record_decision`, `memory.record_edit`,
`memory.feedback`, `memory.status`), four working-memory scratchpad tools
(`memory.scratchpad_write`, `memory.scratchpad_read`,
`memory.scratchpad_grep`, `memory.scratchpad_consolidate`), and four
curation tools that are the reason to pick Lunaris over a vector store —
`memory.verify_agenda` (what the store thinks may be stale),
`memory.resolve` (retire a memory that is superseded),
`memory.dream_agenda` (clusters of raw episodes ripe for distillation) and
`memory.distill` (write the distilled prose back durably). The roster is
pinned by `crates/lunaris-mcp/tests/server_boot.rs::server_boots_and_lists_all_tools`,
which drives the real binary through `tools/list`.
Both install paths download a prebuilt native binary on first run —
no Rust toolchain required (`linux-x64/arm64`, `darwin-x64/arm64`,
`win32-x64`).

**Claude Code** — one command, either runner:

```bash
claude mcp add --transport stdio lunaris -- npx -y @pilotspace/lunaris-mcp
# or
claude mcp add --transport stdio lunaris -- uvx lunaris-mcp
```

**Any MCP client** — JSON config:

```json
{
  "mcpServers": {
    "lunaris": { "command": "npx", "args": ["-y", "@pilotspace/lunaris-mcp"] }
  }
}
```

**From a repo checkout** (adds lifecycle hooks for automatic capture +
context injection, Codex included):

```bash
scripts/setup-lunaris-agents.py --agent both --runner npx   # or: uvx | local
```

**Without a package manager** — build from source:

```bash
cargo install --git https://github.com/pilotspace/lunaris lunaris-mcp
```

`lunaris-mcp` is **not on crates.io** — plain `cargo install lunaris-mcp`
will not work. It links `lunaris-memory-service`, which carries a
`vendor/` path dependency and is therefore `publish = false`; a crate
cannot be published to crates.io while any of its dependencies are
unpublished. The `--git` form above builds the same source (needs a Rust
1.94 toolchain, `cmake`, and a C++ compiler for llama.cpp). From 0.6.1
onward, prebuilt `lunaris-mcp-<target>.tar.gz` binaries are also attached
to each [GitHub release](https://github.com/pilotspace/lunaris/releases).

**`LUNARIS_MCP_STORAGE` is required.** Through 0.6.x an unset value opened a
per-scope SQLite file; 0.7.0 deleted that backend, and the server now refuses
to boot rather than guess a store — a mis-routed memory is harder to notice
than a process that will not start. Point it at a Moon
(`moon://127.0.0.1:6380`), started with `--shards 1`; install Moon via the
Moon repo's curl one-liner or the `ghcr.io/pilotspace/moon` image. A source
build with `--features embedded-moon` auto-launches an in-process Moon — an
opt-in for development, not the published-binary default. First ingest stages
the embedder weights once (lazy GGUF download).

Full guides: [`docs/integration/claude-code.md`](docs/integration/claude-code.md) ·
[`docs/integration/codex.md`](docs/integration/codex.md) ·
[`docs/integration/hooks.md`](docs/integration/hooks.md)

### Tell your AI about Lunaris

Paste this into your `CLAUDE.md` / `AGENTS.md` so your agent uses the
memory deliberately:

```markdown
## Memory (Lunaris MCP)
- Persist durable facts, decisions, and user preferences with
  `memory.ingest`; record code decisions with `memory.record_decision`
  and notable edits with `memory.record_edit`.
- Before answering questions about prior work, query `memory.recall`.
- Use `memory.scratchpad_write`/`scratchpad_read`/`scratchpad_grep` for
  transient working notes within a task (drafts, plans, in-progress state);
  promote the durable ones with `memory.scratchpad_consolidate`.
- Memory is partitioned by scope — never mix scopes; list with
  `memory.list_scopes`. Use `memory.forget` when asked to delete — it
  previews by default; show the match count, then re-issue with
  `dry_run: false` to actually delete.
- Check backend health with `memory.status` if recall returns nothing.
```

## 2. Build with an SDK

```bash
# Python (3.11+)
pip install lunaris

# TypeScript (Node 20+)
npm install @pilotspace/lunaris

# Rust — published as `lunaris-memory`; import as `lunaris`
cargo add lunaris-memory --rename lunaris
```

**Python** — ingest + recall in one file (from
[the SDK guide](docs/book/src/sdk/python.md)):

```python
import asyncio, lunaris, ulid

async def main():
    handle = await lunaris.open("moon://127.0.0.1:6380")

    lsn = await handle.ingest({
        "id": str(ulid.ULID()), "scope": "_dev_", "source": "quickstart",
        "content": "Alice loves chocolate.", "metadata": {}, "t_ref": None,
        "bt": {"valid": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
               "sys":   [{"wall_ms": 0, "counter": 0, "node_id": 0}, None]},
    })

    hits = await handle.recall().query("what does Alice like?").top(5).execute()
    print(lsn, [h["text"] for h in hits])

asyncio.run(main())
```

**TypeScript** — same shape ([SDK guide](docs/book/src/sdk/typescript.md)):

```ts
import { open, RetrievalBuilder } from "@pilotspace/lunaris";

const handle = await open("moon://127.0.0.1:6380");
const lsn = await handle.ingest(episode);           // same episode shape as Python
const hits = await new RetrievalBuilder().bind(handle).top(5).execute();
```

**Rust** — the typed surface:

```rust
use lunaris::{EpisodeBuilder, Lunaris, Scope};

let lunaris = Lunaris::open("moon://127.0.0.1:6380").await?;
let scoped  = lunaris.scoped(Scope::new("acme.agent-1")?);

let lsn = scoped.ingest(EpisodeBuilder::new("user-msg", "Alice loves chocolate.")).await?;
```

`moon://host:port` is the only connection scheme — every retired spelling
(`postgres://`, `memory://`, `sqlite:///path`) returns an error naming the
migration guide. Embedding and reranking run
**in-process** via llama.cpp (`granite-embedding-311m` Q4_K_M +
`bge-reranker-v2-m3` Q5_K_M GGUF) — no embedding API, no network on the
hot path; GPU offload is a build-time `metal`/`cuda`/`vulkan` feature.
Air-gapped options: [configuration reference](docs/book/src/reference/configuration.md).
Memory budgets per build tier (Tier-0 no-inference → full cross-encoder):
[deployment tiers](docs/deployment-tiers.md).

Runnable examples: [`examples/quickstart-py/`](examples/quickstart-py/) ·
[`examples/quickstart-ts/`](examples/quickstart-ts/) ·
[`examples/quickstart-rs/`](examples/quickstart-rs/) ·
[`examples/multi-agent-rs/`](examples/multi-agent-rs/)

## Why Lunaris

Three properties define what Lunaris IS. Every commit is reviewed
against them; any feature that weakens any of the three is rejected.

| Moat | What it means | Where enforced |
|---|---|---|
| **Sub-25 ms p50 recall** | No LLM on the recall hot path. Measured at the 100k-documents-per-scope target corpus: **p50 19.2–22.4 ms · p95 22.3–24.1 ms · p99 23.4–24.4 ms**, engine-side (query embedding excluded), graph OFF, rerank OFF, k=30, single-shard Moon v0.8.5 on an Apple M4 Pro; 500 timed queries after 50 warmup, run-to-run p50 drift ± 3 ms ([envelope + method](docs/operations/capacity.md), [raw samples](docs/benchmarks/ga2b-raw/README.md)). | `scripts/bench/perf/recall_latency.sh all` (local gate, ~10 min, live Moon) |
| **Single `atomic_write` per ingest** | All-or-nothing commit across vector, KV, BM25, graph, audit, queue. Fan-out architectures (Mem0, Zep) can't make this guarantee. | `tests/ingest_pipeline.rs::single_atomic_write_call` + CI grep gate |
| **Bi-temporal MVCC + HLC** | `BiTemporal { valid, sys }` on every primitive, on every backend — `forget` and supersession close intervals instead of destroying rows. As-of *reads* are search-side and graph-side (`FT.SEARCH AS_OF`, `GRAPH.QUERY VALID_AT`); historical **KV** reads are not available on Moon and `read_as_of` refuses rather than answering with today's data — the Postgres/SQLite version chains that served them were removed in 0.7.0 ([limits](docs/ARCHITECTURE.md#honest-limits-read-before-quoting-the-table-above)). | Required field on `Episode`, `Chunk`, `Entity`, `Fact`, `Relation`, `Community` |

## Architecture at a glance

Surface (SDKs / HTTP / MCP / hooks) → engine pipelines (ingest,
retrieval DSL, opt-in graph + consolidation + verification) → one
storage trait → **one backend, Moon** (the trait is still the seam that
kept Postgres and SQLite honest until 0.7.0 removed them). The retrieval
DSL fuses vector,
keyword (BM25), and graph lanes with RRF in a single typed expression —
and on Moon, the fusion and the time-travel cut execute *inside the
substrate*.

Full tour with diagrams: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
and the book's [Architecture at a Glance](docs/book/src/getting-started/architecture.md).

## Why Moon — the substrate advantage

The conventional agent-memory stack is three databases and a broker: a
vector DB, a graph DB, a relational store, and a queue — four failure
domains with no transaction spanning them. Moon collapses all four lanes
into one process, so each Lunaris feature maps onto something the
substrate does *natively* instead of a layer bolted on top:

![What Moon does natively, feature by feature](https://raw.githubusercontent.com/pilotspace/lunaris/main/docs/book/src/images/architecture/moon-feature-superpower.png)

- **Atomic memory** — `TXN.BEGIN` / `TXN.COMMIT` commit every lane at once; no half-written memory.
- **Hybrid recall** — `FT.SEARCH` + native RRF fuse vector + keyword in one round trip.
- **Time-travel** — `FT.SEARCH AS_OF` / `GRAPH.QUERY VALID_AT` make "what did the agent know at T?" a query, not a rebuild (search + graph lanes only; a historical *KV* read has no version chain to walk on Moon, so `read_as_of` refuses explicitly rather than answering with today's data).
- **Opt-in graph** — per-scope `GRAPH.QUERY` (Cypher): relationships without running Neo4j.
- **GDPR forget** — `FT.INVALIDATE_RANGE` erases a whole time range, no scan-and-delete loop.
- **Background work** — a native queue + pub/sub run consolidation without an external broker.

Same job as Mem0, Zep, and Cognee — different *guarantees*. The single
substrate is why several rows below are a ✓ for Lunaris where the
fan-out tools manage only a partial or an ✗:

![Lunaris vs Mem0 / Zep / Cognee, feature by feature](https://raw.githubusercontent.com/pilotspace/lunaris/main/docs/book/src/images/architecture/lunaris-vs-rivals.png)

Every cell is sourced from the comparison table in
[**Why Lunaris**](docs/book/src/getting-started/why-lunaris.md); the full
advantage map — each claim anchored to a code path — lives in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). One honest caveat: plain
key-value point reads aren't natively temporal and index schemas are
fixed at creation, so the architecture page lists every limit beside
every win.

## Recall quality — LongMemEval-S

**Lunaris currently publishes no LongMemEval headline.** The former
`85.4% (427/500)` J-score and its `98.2%` / `93.0%` retrieval companions
were **retracted on 2026-08-21**: the driver that produced them
(`tmp/full500.sh`) was never in git, no raw 500-question artifact set
survives, and the surviving drivers contradict the published methodology
(50 questions per process and a `gpt-4o` judge, against a page claiming
one-process-per-question with a MiniMax judge). Full reasoning:
[`docs/benchmarks/v0.7-longmemeval-jscore-validation.md`](docs/benchmarks/v0.7-longmemeval-jscore-validation.md).

We would rather ship an empty row than an unreproducible one. What
exists instead, today:

- **The harness is in the repository** — `scripts/bench/lme/`, one
  process per question, config-fingerprinted, resume-safe, with its own
  reserved-port guards. Ported into version control in 0.6.2 precisely
  so that a number can never again outlive its runner.
- **A judge-free retrieval ratchet runs in CI** on every recall-affecting
  push to `main` (LongMemEval-S evidence-recall *any-gold*, deterministic,
  no API key), gated against a checked-in baseline that records the exact
  retrieval config it was measured under. Its scope, its current fail
  floor and the work to give it teeth are documented — including the
  parts that do not yet work — in
  [`scripts/bench/lme/baselines/README.md`](scripts/bench/lme/baselines/README.md).
- **A full N=125 A/B re-run on the committed harness is pending**, blocked
  on a provider key. When it lands, its raw per-question artifacts get
  committed under [`docs/benchmarks/lme-raw/`](docs/benchmarks/lme-raw/README.md)
  in the same shape as the latency envelope's — because
  [a published number without a committed raw artifact is not publishable
  here](docs/benchmarks/README.md).

Competitor LongMemEval figures previously listed in this table (Zep,
Mem0) were removed at the same time: they carried no citation, and one of
them was a LoCoMo score mislabelled as LongMemEval. See
[**Competitor figures**](docs/benchmarks/README.md#competitor-figures).

## Persona tracking — PersonaMem (32k)

Full 32k split (589 questions, 37 shared contexts), production hybrid
recall path on the **quality operating point** (rerank ON — *not* the
shipped default; see
[operating points](docs/benchmarks/operating-points.md)), exact
letter-match scoring (no LLM judge), zero errors:

| Configuration | Accuracy |
|---|---|
| **Lunaris + claude-sonnet-5 reader** (single reader, quality path) | **75.0%** (442/589) |
| No-memory floor (same reader, options only) | 41.9% (247/589) |
| TencentDB-Agent-Memory (published; split/reader unstated) | 76% / 48% |
| *Two-reader oracle cascade — an upper bound, not a system result¹* | *81.8% (482/589)* |

**Memory lift: +33.1 points** with the identical reader — larger than
Tencent's published +28, from a lower floor. Fact-recall questions go
from 2.3% without memory to **78.3%** with it, single reader.

Two caveats we state rather than bury:

¹ **81.8% is an oracle bound, not a measurement of Lunaris.**
claude-opus-5 re-answered only the questions the Sonnet arm missed, and
*gold labels* decided which questions those were. A deployable cascade
would need a gold-free routing rule. The headline is the clean
single-reader **75.0%**.

² **One of the seven categories does not measure memory.** In
`suggest_new_ideas` (93 of the 589 questions) the gold answer is
essentially always the *shortest* option — a classifier that reads
nothing and picks the shortest candidate scores **98.9%** there against
0–15.5% in every other category. Its published "memory net-harms this
category" reading is an artifact of that, not a finding, and we
deliberately do **not** optimise against it. The 75.0% is understated by
the presence of 93 questions that cannot be won on merit. Root cause and
the regression test that pins it: [issue
#141](https://github.com/pilotspace/lunaris/issues/141).

Full methodology, per-category table, caveats, and reproduction commands:
[`scripts/bench/pm/RESULTS.md`](scripts/bench/pm/RESULTS.md) and the
[book write-up](https://github.com/pilotspace/lunaris/blob/main/docs/book/src/benchmarks/personamem.md).

## Multi-agent isolation

Every Lunaris operation is partitioned by `Scope` — a validated newtype
enforced at compile time and at the storage boundary (per-scope Moon
keyspaces + per-scope indices). Cross-scope reads are
a type error. See [RFC 0001](docs/rfcs/0001-scope-newtype.md).

```rust
let scope_a = Scope::new("acme.agent-1")?;
let scope_b = Scope::new("acme.agent-2")?;

// Same ULID, different scopes — two distinct rows. No leak.
lunaris.scoped(scope_a).ingest(builder.clone()).await?;
lunaris.scoped(scope_b).ingest(builder).await?;
```

## Operating in production

External Moon is the supported deployment (the embedded server is dev/test-only):

- [`docs/operations/external-moon.md`](docs/operations/external-moon.md) — zero-to-connected: install, required version (≥ 0.8.5, enforced by a connect-time handshake), AOF persistence, **single-shard only**
- [`docs/operations/backup-restore.md`](docs/operations/backup-restore.md) — the drilled backup/restore-to-new-host runbook with measured RPO/RTO
- [`docs/operations/observability.md`](docs/operations/observability.md) — `/metrics`, `/readyz` semantics, Prometheus scrape config + starter alerts
- [`deploy/`](deploy/) — docker-compose (Moon + lunaris-server with health-checked readiness) and the server Dockerfile

## Status

**Current release: 0.7.0** (2026-08-18). Newest first; every row is a git
tag. [`CHANGELOG.md`](CHANGELOG.md) is the authority — this table is a summary.

| Release | Date | What landed |
|---|---|---|
| **0.7.0 — Moon-only, and the GA cut** | 2026-08-18 | Every storage backend except Moon deleted; one `production_root` recall plan across HTTP / SDK / MCP / hook; opt-in rerank stage; recall-ratchet CI gate (replacing the Eval Gauntlet, which had 20 startup failures and 0 completed runs); measured 100k-doc capacity envelope with committed raw artifacts; rehearsed upgrade/rollback |
| **0.6.2 — operability** | 2026-08-15 | Last release shipping Postgres + SQLite. Historical `read_as_of` / `scan_range` on Moon now fail loudly instead of quietly returning present-time data |
| **0.6.0-rc.1 / rc.2** | 2026-07-15 / 07-17 | llama.cpp-only cutover (candle deleted); adaptive chunking + RAPTOR tree retrieval; Moon v0.8.0 bump |
| **0.5.0 — adapters + memory convergence** | 2026-06-16 | LangGraph / CrewAI / Letta reference adapters, write-time dedup + cross-episode supersede, relicensed Apache-2.0 |
| **0.4.0 — MCP surface** | 2026-06-13 | `lunaris-mcp` + the `memory.scratchpad_*` tools, RAPTOR tree retrieval, recall fan-out p50 12 → 6 ms, hybrid filter push-down |
| **0.3.0 — proactive capture + packaging** | 2026-06-05 | `lunaris-hook` lifecycle capture, MCP polish, npx / uvx distribution |
| **0.2.1 — multi-agent partitioning** | 2026-05-12 | The `Scope` newtype partition key |
| **0.1.x** | 2026-04 → 05 | First engine cut: bi-temporal store, retrieval DSL, single-`atomic_write` ingest |

[`RELEASES.md`](RELEASES.md) carries the per-release gate evidence.

## Contributing

[`CONTRIBUTING.md`](CONTRIBUTING.md) has the build + test recipe (including
the `MOON_TEST_BINARY` that storage-backed tests need), what CI checks, and
the grep-pinned invariants a PR must not break. Participation is governed by
our [Code of Conduct](CODE_OF_CONDUCT.md). Security issues go to
[`SECURITY.md`](SECURITY.md), never a public issue.

## Coming from another agent-memory tool?

- **[`docs/MIGRATING-FROM-MEM0.md`](docs/MIGRATING-FROM-MEM0.md)** —
  code-side comparisons (ingest, recall, time-travel, forget), a 5-step
  incremental migration plan, honest "stay on Mem0 if…" criteria.
- **[`docs/MIGRATING-FROM-ZEP.md`](docs/MIGRATING-FROM-ZEP.md)** — Zep
  already has bi-temporal facts; the conversation is latency + substrate
  simplification.
- **[`docs/MIGRATING-FROM-COGNEE.md`](docs/MIGRATING-FROM-COGNEE.md)** —
  pipeline-vs-DSL tradeoff: if your custom logic lives at ingest time,
  Cognee's Task model maps cleaner; at recall time, Lunaris's operator
  DSL is simpler.

## For contributors

- **[`CLAUDE.md`](CLAUDE.md)** — engineering constraints (Rust edition, MSRV 1.94, file size, lock discipline).
- **[`docs/rfcs/`](docs/rfcs/)** — design contracts. 0001 (Scope) shipped; 0004 / 0006 / 0007 in Draft.
- **[`docs/migration/`](docs/migration/)** — upgrade guides, including the
  [0.3 → 0.4 native-default cutover](docs/migration/0.3-to-0.4-native-default.md)
  and the [0.4 → 0.5 release notes](docs/migration/0.4-to-0.5.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE). See [`LICENSE`](LICENSE)
for the full text.
