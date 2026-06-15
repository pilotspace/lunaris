# Why Lunaris

**Read this before evaluating Lunaris.** It is the one-page elevator
pitch plus the honest "use a different tool when…" criteria — the page a
maintainer points at when someone asks "how does this compare to X?" The
[migration chapters](../migrating/mem0.md) go deeper; this page is the
pitch and the exclusions.

## The one-line claim

**Sub-25 ms recall over millions of bi-temporal facts, with provable
atomicity and an opt-in graph.** Embedded Rust core. Python + TypeScript
bindings generated from the same source of truth. Apache 2.0. That's it.

You feed it raw observations — chat turns, documents, tool outputs — as
`Episode`s. It chunks, embeds, and (optionally) extracts entities,
relations, and facts using a small local LLM, then stores everything in a
bi-temporal MVCC store backed by **Postgres** (the portable default) or
**Moon** (a high-performance Redis-compatible substrate). Agents query it
through a composable retrieval DSL that fuses semantic search, graph
traversal, and BM25 keyword lookup, with an optional cross-encoder rerank
pass on top.

## The three moats

Three properties define what Lunaris **is**. Every commit is reviewed
against them; any feature that weakens any of the three is rejected.

| Moat | What it means | Where enforced |
|---|---|---|
| **Sub-25 ms p50 recall** | No LLM on the recall hot path. Cross-encoder rerankers stay sub-30 ms. | `cargo bench --bench recall_hot_path` + perf smoke in CI |
| **Single `atomic_write` per ingest** | All-or-nothing commit across vector, KV, BM25, audit, and queue. Fan-out architectures (Mem0, Zep) can't make this guarantee. | `crates/lunaris-ingest/tests/ingest_pipeline.rs::single_atomic_write_call` + CI grep gate |
| **Bi-temporal MVCC + HLC** | `BiTemporal { valid, sys }` on every primitive. "What did the agent know at time T" is a query, not a rebuild. | Required field on `Episode`, `Chunk`, `Entity`, `Fact`, `Relation`, `Community` (`crates/lunaris-core/src/bitemporal.rs`) |

If everything else fails, that performance + correctness contract must
hold — it's what differentiates Lunaris from Mem0, Zep, and Cognee.

A few more differentiators, with proof:

| Differentiator | What it gets you | Proof source |
|---|---|---|
| **Composable retrieval DSL** | `Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(5)` is one typed expression. Hybrid search isn't a feature flag; it's an operator combinator. | `crates/lunaris-retrieve/src/builder.rs` |
| **Type-enforced multi-tenancy** | `Scope::new(s)?` validates against `[A-Za-z0-9_\-.]{1,128}`; the wire can't smuggle a different scope past `ScopedLunaris`. Postgres RLS enforces it again at the database boundary. | `crates/lunaris-core/src/scope.rs` ([RFC 0001](https://github.com/pilotspace/lunaris/blob/main/docs/rfcs/0001-scope-newtype.md)) |
| **Opt-in graph** | `Graph::anchored(entity_ids, hops)` is an operator. Off by default — your dev box doesn't load a graph extractor until you call `lunaris.graph_pipeline().enable()`. | `crates/lunaris-retrieve/src/operators/graph.rs` |
| **Pluggable verifier with a laptop floor** | RFC 0006's 27B → 270M flip: the `verify-small` build runs a *real* slow-path verifier in ~600 MB disk / ~1 GB RAM. With stock features and no weights staged the effective verifier is `NoopVerifier` (a `tracing::warn!` says so) — opt in deliberately. | `crates/lunaris-verify/src/candle_gemma3_270m.rs` |
| **One substrate, not three** | Moon *or* Postgres holds the vector index, the graph, the keyword index, and the queue. No vector DB + graph DB + relational DB to operate. | [Choosing a Backend](../operations/backends.md) |

## When Lunaris is the answer

Pick Lunaris when you can say **yes** to most of these:

- My agent's recall p50 is on the hot path of user experience —
  300 ms feels slow.
- I want a single substrate (Moon or Postgres) instead of running a
  vector DB + graph DB + relational DB.
- I need bi-temporal queries: "what did the agent believe at time T?"
- I need multi-tenant isolation that the type system enforces, not just
  a `user_id` string the caller could swap.
- I want a composable retrieval DSL where vector + keyword + graph fuse
  in one typed expression, not three API calls glued together.
- My stack is Rust, or Python with a Rust binary in the build chain is
  acceptable, or TypeScript with a NAPI binding is acceptable.
- Apache 2.0 + open source matters; I want to read the substrate code,
  not depend on a hosted service.
- **I need a hosted SaaS with zero infra ownership.** - Lunaris-cloud in progress (shared and managed memories in graph)
- **My recall latency budget is 500+ ms anyway.** The 25 ms contract
  isn't free to operate; if you don't need it, pick the hosted option.

## How Lunaris compares to Mem0 / Zep / Cognee

| | **Lunaris** | **Mem0** | **Zep** | **Cognee** |
|---|---|---|---|---|
| Core language | Rust (Py + TS bindings) | Python | Python / Go | Python |
| Recall latency contract | sub-25 ms p50, enforced in CI | best-effort | best-effort | best-effort |
| Atomic ingest | single `atomic_write`, all-or-nothing | fan-out writes | fan-out writes | task pipeline |
| Bi-temporal | yes, at the storage layer (`valid` + `sys`) | no | yes | partial |
| Substrate count | 1 (Moon *or* Postgres) | vector DB + store | vector DB + graph DB | configurable, multi |
| Hybrid retrieval | typed DSL: `vector.and(keyword).fuse_rrf().top()` | flag | flag | pipeline step |
| Graph | opt-in operator, off by default | Mem0g (Platform-only) | always-on | pipeline-driven |
| Multi-tenancy | `Scope` newtype + Postgres RLS | `user_id` string | `session_id` | namespace |
| Hosted option | not yet (roadmap) | yes | yes (Zep Cloud) | self-host |
| License | Apache 2.0 | Apache 2.0 | Apache 2.0 | Apache 2.0 |

Already running one of these? The migration chapters walk through
ingest, recall, time-travel, and forget — code-side, with honest "stay
on $incumbent if…" criteria:

- **[Migrating from Mem0](../migrating/mem0.md)** — Mem0 has no
  bi-temporal layer, so the migration *is* the bi-temporal upgrade.
- **[Migrating from Zep](../migrating/zep.md)** — Zep already has
  bi-temporal facts, so the conversation is latency + substrate
  simplification (1 service vs 2).
- **[Migrating from Cognee](../migrating/cognee.md)** — Cognee is
  pipeline-oriented; Lunaris is operator-oriented. The question is
  "where does your domain logic live — ingest time or recall time?"

## What's still on the runway

`v0.2.x` is the OSS-publish milestone, not the end state:

- **v0.3 self-hosted** — Docker / Helm, SLOs, design partners. The
  hosted-substrate experience for teams that want Lunaris but don't want
  to operate Moon themselves. Also lands the typed `Scope` +
  `EpisodeBuilder` surface in the Python / TS SDKs and per-scope
  `ScopedLunaris::forget`.
- **Ecosystem (shipped)** — LangGraph / CrewAI / Letta adapters via the
  `lunaris-integrations` package (`pip install lunaris-integrations[langgraph]`,
  `[crewai]`, `[letta]`). The "drop Lunaris into your existing agent framework"
  experience: LangGraph and CrewAI are drop-in store / storage classes; Letta
  ships as a client-backed connector shim + recipe (its archival store is
  server-side). The MCP server remains the universal shim for frameworks
  without a dedicated adapter.

## Where to go next

- **Try it now** → [Installation](./installation.md), then the
  [10-Minute Quickstart](./quickstart.md).
- **Understand the model** → [Core Concepts](./concepts.md).
- **Evaluate against your corpus** → read the moat table above + the
  migration chapter for your current tool, then run
  [`examples/quickstart-py/`](https://github.com/pilotspace/lunaris/tree/main/examples/quickstart-py)
  against your real data.
