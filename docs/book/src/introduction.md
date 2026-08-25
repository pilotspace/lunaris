<div class="lx-hero">
  <p class="lx-eyebrow">Agent Memory Engine</p>
  <h1 class="lx-hero-title">🌙 Lunaris</h1>
  <p class="lx-hero-sub">Sub-25 ms recall at 100,000 documents per scope — measured — with provable atomicity and a graph that's opt-in.</p>
  <div class="lx-cta">
    <a class="lx-btn lx-btn-primary" href="getting-started/quickstart.html">Get started</a>
    <a class="lx-btn lx-btn-ghost" href="https://github.com/pilotspace/lunaris">Our GitHub</a>
  </div>
</div>

<div class="lx-cards">
  <a class="lx-card" href="getting-started/quickstart.html"><span class="lx-card-icon">🚀</span><span class="lx-card-title">10-Minute Quickstart</span><span class="lx-card-desc">Zero to recall in ten minutes — no external services required.</span></a>
  <a class="lx-card" href="getting-started/why-lunaris.html"><span class="lx-card-icon">🧭</span><span class="lx-card-title">Why Lunaris</span><span class="lx-card-desc">The three moats, and the honest "use something else when…" criteria.</span></a>
  <a class="lx-card" href="getting-started/architecture.html"><span class="lx-card-icon">🏛️</span><span class="lx-card-title">Architecture</span><span class="lx-card-desc">How sub-25 ms recall and one-atomic-write ingest fit together.</span></a>
  <a class="lx-card" href="guides/retrieval-dsl.html"><span class="lx-card-icon">🧩</span><span class="lx-card-title">Retrieval DSL</span><span class="lx-card-desc">Vector + BM25 + graph + rerank composed as one typed expression.</span></a>
  <a class="lx-card" href="mcp/index.html"><span class="lx-card-icon">🔌</span><span class="lx-card-title">MCP Server</span><span class="lx-card-desc">Plug Lunaris memory into Claude Code, Codex, and any MCP agent.</span></a>
  <a class="lx-card" href="cookbook/index.html"><span class="lx-card-icon">📚</span><span class="lx-card-title">Cookbook</span><span class="lx-card-desc">Copy-pasteable recipes: chat agents, docs, Slack archives, timelines.</span></a>
</div>

Lunaris is a production-grade **agent-memory engine** written in pure Rust,
with first-class **Python** (`pip install lunaris`) and **TypeScript**
(`npm i @pilotspace/lunaris`) SDKs generated from the same source of truth.

You feed it raw observations — chat turns, documents, tool outputs — as
`Episode`s. It chunks, embeds, and (optionally) extracts entities, relations,
and facts using a small local LLM, then stores everything in a bi-temporal
MVCC store backed by **Moon**, a high-performance Redis-compatible
substrate (and, as of 0.7.0, the only backend). Agents query it through a
composable retrieval DSL that fuses semantic search, graph traversal, and
BM25 keyword lookup, with an optional cross-encoder rerank pass on top.

```rust,no_run
# async fn demo() -> Result<(), lunaris::LunarisError> {
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
use lunaris::{EpisodeBuilder, Lunaris, Query, Scope};

let lunaris = Lunaris::open("moon://127.0.0.1:6380").await?;
let scope   = Scope::new("acme.agent-1")?;
let scoped  = lunaris.scoped(scope);

let lsn  = scoped.ingest(EpisodeBuilder::new("user-msg", "Alice loves chocolate.")).await?;
let hits = scoped.recall(Query::text("what does Alice like?")).await?;
# Ok(())
# }
# Ok(())
# }
```

Want a hybrid plan (vector + BM25, fused, reranked)? Compose it with the
[retrieval DSL](./guides/retrieval-dsl.md):

```rust,no_run
# use lunaris::{Lunaris, Scope};
# async fn demo() -> Result<(), lunaris::LunarisError> {
# use lunaris::{Lunaris, Scope};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Lunaris::open("moon://127.0.0.1:6380").await?;
# let scoped = lunaris.scoped(Scope::new("acme.agent-1")?);
use lunaris::{Keyword, Query, Vector};

let hits = scoped
    .dsl()
    .with_root(Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(5))
    .execute(Query::text("what does Alice like?"))
    .await?;
# Ok(())
# }
# Ok(())
# }
```

## Use it from your agent (MCP)

Don't want to write SDK code? Lunaris ships an **MCP server** so coding agents
— [Claude Code](./mcp/claude-code.md), [Codex](./mcp/codex.md), or any MCP
client — get persistent, scope-isolated memory over stdio. Install the binary
and register it:

```sh
# no Rust toolchain needed — both download a prebuilt binary on first run
claude mcp add --transport stdio lunaris \
  -e LUNARIS_MCP_STORAGE=moon://127.0.0.1:6380 \
  -- npx -y @pilotspace/lunaris-mcp
# or: claude mcp add --transport stdio lunaris \
  -e LUNARIS_MCP_STORAGE=moon://127.0.0.1:6380 \
  -- uvx lunaris-mcp
```

Building from source instead? `lunaris-mcp` is **not on crates.io** (it
depends on a `publish = false` crate), so use the git form:
`cargo install --git https://github.com/pilotspace/lunaris lunaris-mcp`.

The agent then calls eleven `memory.*` tools — seven durable-memory tools
(`ingest`, `recall`, `forget`, `list_scopes`, `record_decision`,
`record_edit`, `status`) plus four working-memory scratchpad tools
(`scratchpad_write`, `scratchpad_read`, `scratchpad_grep`,
`scratchpad_consolidate`):

```text
memory.ingest  source="src:notes"  content="The ingest pipeline writes one atomic_write per episode."
memory.recall  query="ingest atomicity"  k=3
```

Scope is derived per-repo from the git remote. `LUNARIS_MCP_STORAGE` must name
a Moon — there is no default store as of 0.7.0, and the server refuses to boot
without one. `memory.recall` then runs hybrid vector + BM25 recall. See
**[MCP Server](./mcp/index.md)** for the full guide.

## The three moats

Three properties define what Lunaris **is**. Every commit is reviewed against
them; any feature that weakens any of the three is rejected.

| Moat | What it means | Where enforced |
|---|---|---|
| **Sub-25 ms p50 recall** | No LLM on the recall hot path. Measured p50 19.2–22.4 ms / p99 23.4–24.4 ms at 100k documents per scope, graph OFF, rerank OFF. The opt-in cross-encoder rerank is a **quality** stage, not a latency-class stage — it measures **p50 1301.3 ms** at `top_in=60` and voids this contract when enabled. | `scripts/bench/perf/recall_latency.sh all` — a **manual, local** ~10-minute live-Moon gate. **Not CI-enforced:** `perf-gates.yml` is opt-in behind a `perf-bench` label, is not a required check, and is red on main ([`capacity.md`](https://github.com/pilotspace/lunaris/blob/main/docs/operations/capacity.md)) |
| **Single `atomic_write` per ingest** | All-or-nothing commit across vector, KV, BM25, queue. Fan-out architectures (Mem0, Zep) can't make this guarantee. | `tests/ingest_pipeline.rs::single_atomic_write_call` + CI grep gate |
| **Bi-temporal MVCC + HLC** | `BiTemporal { valid, sys }` on every primitive, on every backend — supersede closes intervals instead of destroying rows. As-of *reads* are search-side and graph-side (`FT.SEARCH AS_OF`, `GRAPH.QUERY VALID_AT`); historical **KV** reads have no version chain on Moon and are refused explicitly — see [Core concepts](./getting-started/concepts.md#bi-temporal-mvcc--the-hlc). | Required field on `Episode`, `Chunk`, `Entity`, `Fact`, `Relation`, `Community` |

If everything else fails, that performance + correctness contract must hold —
it's what differentiates Lunaris from Mem0, Zep, and Cognee. See
[Why Lunaris](./getting-started/why-lunaris.md) for the honest "use a
different tool when…" criteria.

## How this book is organized

- **[Getting Started](./getting-started/installation.md)** — install, a
  10-minute quickstart, and the core concepts (episodes, scope, bi-temporal
  MVCC, the atomic write).
- **[Guides](./guides/ingest.md)** — one chapter per capability: ingest, the
  retrieval DSL, forget, the opt-in graph pipeline, consolidation &
  verification, multi-agent scoping.
- **[Cookbook](./cookbook/index.md)** — the built-in recipe types
  (chat agents, document corpora, Slack/email archives, code-repo memory,
  timelines) as copy-pasteable how-tos.
- **[Reference](./reference/configuration.md)** — the exhaustive
  configuration reference (every feature flag and `LUNARIS_*` env var), the
  generated API docs, and the error taxonomy.
- **[Operations](./operations/server.md)** — running the HTTP server,
  choosing a backend, durability & recovery.
- **[SDKs](./sdk/python.md)** — Python and TypeScript surface notes.
- **[Integrations (MCP)](./mcp/index.md)** — the MCP server and its Claude
  Code / Codex integration guides.
- **[Migrating From](./migrating/mem0.md)** — Mem0 / Zep / Cognee mapping
  tables.
- **[Protocol](./protocol/memoryprotocol-0.1.md)** — the MemoryProtocol 0.1
  HTTP/SSE wire spec and its conformance suite.

> **Source of truth.** Where a claim in this book disagrees with the Rust
> source, the source wins. Many pages carry `path:line` cross-references back
> into the crates; the generated [API reference](./reference/api.md) is built
> from `cargo doc` on every release.
