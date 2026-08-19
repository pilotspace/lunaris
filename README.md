# Lunaris

**Sub-25 ms recall over millions of bi-temporal facts, with provable
atomicity and a graph that's opt-in.**

A production-grade agent-memory engine in Rust, with first-class Python
and TypeScript SDKs and a zero-Rust MCP server for coding agents. Raw
observations in; structured, bi-temporal facts out. Backed by **Moon**, the
high-performance Redis-compatible substrate — and, as of 0.7.0, only Moon.
The Postgres and SQLite backends were removed; see
[0.6 → 0.7](docs/migration/0.6-to-0.7.md) if you are on one.

![Lunaris layered architecture](https://raw.githubusercontent.com/pilotspace/lunaris/main/docs/book/src/images/architecture/lunaris-layers.png)

> **Documentation:** the full guide lives in the **[Lunaris Book](https://pilotspace.github.io/lunaris/)** or
> (`mdbook serve docs/book` locally .
> live). **First time here?** [`docs/POSITIONING.md`](docs/POSITIONING.md) is
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

The MCP server gives any MCP-capable agent eleven memory tools — seven
durable-memory tools (`memory.ingest`, `memory.recall`, `memory.forget`,
`memory.list_scopes`, `memory.record_decision`, `memory.record_edit`,
`memory.status`) plus four working-memory scratchpad tools
(`memory.scratchpad_write`, `memory.scratchpad_read`,
`memory.scratchpad_grep`, `memory.scratchpad_consolidate`).
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
        "id": str(ulid.ULID()), "source": "quickstart",
        "content": "Alice loves chocolate.", "metadata": {}, "t_ref": None,
        "bt": {"valid": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
               "sys":   [{"wall_ms": 0, "counter": 0, "node_id": 0}, None]},
    })

    hits = await lunaris.RetrievalBuilder().bind(handle).top(5).execute()
    print(lsn, hits)

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
| **Sub-25 ms p50 recall** | No LLM on the recall hot path. Measured strict-replay: p50 10.3 ms / p99 20.8 ms ([methodology](docs/benchmarks/v0.2.x/README.md)); k=30 hydration tail p50 6.0 ms / p99 6.2 ms after the concurrent-hydration fan-out ([A/B](docs/benchmarks/v0.6-recall-fanout-ab.md)). | `cargo bench --bench recall_hot_path` |
| **Single `atomic_write` per ingest** | All-or-nothing commit across vector, KV, BM25, graph, audit, queue. Fan-out architectures (Mem0, Zep) can't make this guarantee. | `tests/ingest_pipeline.rs::single_atomic_write_call` + CI grep gate |
| **Bi-temporal MVCC + HLC** | `BiTemporal { valid, sys }` on every primitive, on every backend — `forget` and supersession close intervals instead of destroying rows. As-of *reads* are search-side and graph-side (`FT.SEARCH AS_OF`, `GRAPH.QUERY VALID_AT`); historical **KV** reads are not available on Moon and `read_as_of` refuses rather than answering with today's data — the Postgres/SQLite version chains that served them were removed in 0.7.0 ([limits](docs/ARCHITECTURE.md#honest-limits-read-before-quoting-the-table-above)). | Required field on `Episode`, `Chunk`, `Entity`, `Fact`, `Relation`, `Community` |

## Architecture at a glance

Surface (SDKs / HTTP / MCP / hooks) → engine pipelines (ingest,
retrieval DSL, opt-in graph + consolidation + verification) → one
storage trait → three backends. The retrieval DSL fuses vector,
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

Full `longmemeval_s` dataset (N=500, not a subsample), `main` HEAD, one
process per question:

| Metric | Score |
|---|---|
| **Retrieval — any gold session surfaced** | **98.2% (491/500)** |
| **Retrieval — all gold sessions surfaced** | **93.0% (465/500)** |
| End-to-end J-score (MiniMax m3 as both generator and judge) | 85.4% (427/500) |
| Zep (published, GPT-4o reader) | 90.2% |
| Mem0 (published) | 66–68% |

Retrieval itself is strong and holds up at full scale. The end-to-end
J-score sits below retrieval recall because most misses are the reader
model reasoning incorrectly over evidence it *did* retrieve (multi-session
counting/aggregation questions), not Lunaris failing to find the right
memories — see the miss breakdown in
[`docs/benchmarks/v0.7-longmemeval-jscore-validation.md`](docs/benchmarks/v0.7-longmemeval-jscore-validation.md).
The Zep/Mem0 figures use their own (different) reader models, so the
end-to-end row is not a controlled apples-to-apples comparison — it's
included for context, not as a head-to-head claim.

## Persona tracking — PersonaMem (32k)

Full 32k split (589 questions, 37 shared contexts), production hybrid
recall path, exact letter-match scoring (no LLM judge), zero errors:

| Configuration | Accuracy |
|---|---|
| **Lunaris + two-reader ensemble (oracle upper bound¹)** | **81.8%** (482/589) |
| Lunaris + claude-sonnet-5 reader | **75.0%** (442/589) |
| No-memory floor (same reader, options only) | 41.9% (247/589) |
| TencentDB-Agent-Memory (published; split/reader unstated) | 76% / 48% |

**Memory lift: +33.1 points** with the identical reader — larger than
Tencent's published +28, from a lower floor. Fact-recall questions go
from 2.3% without memory to 83.7% with it.

¹ claude-opus-5 re-answered only the questions the Sonnet arm missed
(gold labels routed them), so 81.8% is an upper bound on a two-reader
cascade, not a single-reader measurement; the clean single-reader number
is 75.0%. Full methodology, per-category table, caveats, and
reproduction commands:
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

| Milestone | Status |
|---|---|
| **v0.2.1 — multi-agent partitioning** | Shipped 2026-05-11 |
| **v0.4 — in-process ML default** | Shipped 2026-05-14 — in-process embedder + reranker, Ollama path removed (candle then; llama.cpp since v0.6) |
| **v0.4.0 — MCP surface + embedded Moon + RAPTOR** | Shipped 2026-06-13 — `lunaris-mcp` scratchpad tools, embedded Moon, RAPTOR tree retrieval, recall fan-out p50 12→6 ms, hybrid filter push-down |
| **v0.5.0 — framework adapters + memory convergence + Apache-2.0** | Shipped 2026-06-16 — `lunaris_integrations` LangGraph/CrewAI/Letta adapters, write-time dedup + cross-episode supersede, relicensed Apache-2.0 |
| **v0.5 — proactive capture + packaging** | Shipped 2026-05-26 — `lunaris-hook` lifecycle capture, MCP polish, npx/uvx distribution |
| **v0.6 — adaptive chunking + RAPTOR** | In progress on `main` — hierarchical memory tree, `.tree()` retrieval operator |
| **MCP working memory + embedded Moon** | Merged to `main` 2026-06-09 — four `memory.scratchpad_*` tools, guarded `scratchpad_consolidate`, opt-in `--features embedded-moon` |

See [`CHANGELOG.md`](CHANGELOG.md) for the full history.

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
