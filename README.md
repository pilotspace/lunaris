# Lunaris

**Sub-25 ms recall over millions of bi-temporal facts, with provable
atomicity and a graph that's opt-in.**

A production-grade agent-memory engine in Rust, with first-class Python
and TypeScript SDKs and a zero-Rust MCP server for coding agents. Raw
observations in; structured, bi-temporal facts out. Backed by **Moon**
(the high-performance Redis-compatible substrate), **Postgres**, or
**SQLite** (`memory://` — zero infrastructure).

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

The MCP server gives any MCP-capable agent seven memory tools:
`memory.ingest`, `memory.recall`, `memory.forget`, `memory.list_scopes`,
`memory.record_decision`, `memory.record_edit`, `memory.status`.
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

Storage defaults to Moon at `moon://127.0.0.1:6380` with a per-scope
SQLite fallback (`--storage-backend sqlite`) — recall works out of the
box on SQLite via brute-force cosine; switch to Moon or Postgres for
HNSW-class latency beyond ~10k vectors per scope. First ingest stages
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
- Memory is partitioned by scope — never mix scopes; list with
  `memory.list_scopes`. Use `memory.forget` when asked to delete.
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
    handle = await lunaris.open("moon://127.0.0.1:6380")  # or "memory://"

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

const handle = await open("moon://127.0.0.1:6380"); // or "memory://"
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

The connection URL is the only backend switch: `moon://` (latency
flagship), `postgres://` (portability, RLS-isolated), `memory://` /
`sqlite:///path` (zero infrastructure). Embedding and reranking run
**in-process on CPU** (candle-native `granite-embedding-311m` +
`bge-reranker-v2-m3`) — no embedding API, no network on the hot path;
the model weights are staged once on first use. Quantized GGUF and
air-gapped options: [configuration reference](docs/book/src/reference/configuration.md).

Runnable examples: [`examples/quickstart-py/`](examples/quickstart-py/) ·
[`examples/quickstart-ts/`](examples/quickstart-ts/) ·
[`examples/quickstart-rs/`](examples/quickstart-rs/) ·
[`examples/multi-agent-rs/`](examples/multi-agent-rs/)

## Why Lunaris

Three properties define what Lunaris IS. Every commit is reviewed
against them; any feature that weakens any of the three is rejected.

| Moat | What it means | Where enforced |
|---|---|---|
| **Sub-25 ms p50 recall** | No LLM on the recall hot path. Measured strict-replay: p50 10.3 ms / p99 20.8 ms ([methodology](docs/benchmarks/v0.2.x/README.md)). | `cargo bench --bench recall_hot_path` |
| **Single `atomic_write` per ingest** | All-or-nothing commit across vector, KV, BM25, graph, audit, queue. Fan-out architectures (Mem0, Zep) can't make this guarantee. | `tests/ingest_pipeline.rs::single_atomic_write_call` + CI grep gate |
| **Bi-temporal MVCC + HLC** | `BiTemporal { valid, sys }` on every primitive. "What did the agent know at time T" is a query, not a rebuild. | Required field on `Episode`, `Chunk`, `Entity`, `Fact`, `Relation`, `Community` |

## Architecture at a glance

Surface (SDKs / HTTP / MCP / hooks) → engine pipelines (ingest,
retrieval DSL, opt-in graph + consolidation + verification) → one
storage trait → three backends. The retrieval DSL fuses vector,
keyword (BM25), and graph lanes with RRF in a single typed expression —
and on Moon, the fusion and the time-travel cut execute *inside the
substrate*.

Full tour with diagrams: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
and the book's [Architecture at a Glance](docs/book/src/getting-started/architecture.md).

## Multi-agent isolation

Every Lunaris operation is partitioned by `Scope` — a validated newtype
enforced at compile time and at the storage boundary (Postgres RLS with
`WITH CHECK`, per-scope Moon keyspaces + indices). Cross-scope reads are
a type error. See [RFC 0001](docs/rfcs/0001-scope-newtype.md).

```rust
let scope_a = Scope::new("acme.agent-1")?;
let scope_b = Scope::new("acme.agent-2")?;

// Same ULID, different scopes — two distinct rows. No leak.
lunaris.scoped(scope_a).ingest(builder.clone()).await?;
lunaris.scoped(scope_b).ingest(builder).await?;
```

## Status

| Milestone | Status |
|---|---|
| **v0.2.1 — multi-agent partitioning** | Shipped 2026-05-11 |
| **v0.4 — candle-native ML default** | Shipped 2026-05-14 — in-process embedder + reranker, Ollama path removed |
| **v0.4 wave-a — `lunaris-mcp`** | Shipped 2026-05-24 — stdio MCP for Claude Code / Codex |
| **v0.5 — proactive capture + packaging** | Shipped 2026-05-26 — `lunaris-hook` lifecycle capture, MCP polish, npx/uvx distribution |
| **v0.6 — adaptive chunking + RAPTOR** | In progress on `main` — hierarchical memory tree, `.tree()` retrieval operator |

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
  [0.3 → 0.4 native-default cutover](docs/migration/0.3-to-0.4-native-default.md).

## License

Dual-licensed under [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT),
at your option. See [`LICENSE`](LICENSE).
