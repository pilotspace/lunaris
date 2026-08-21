# Why Lunaris

**Read this before evaluating Lunaris.** It is the one-page elevator
pitch plus the honest "use a different tool when…" criteria — the page a
maintainer points at when someone asks "how does this compare to X?" The
[migration chapters](../migrating/mem0.md) go deeper; this page is the
pitch and the exclusions.

## The one-line claim

**Sub-25 ms recall at 100,000 documents per scope, with provable
atomicity and an opt-in graph.** Embedded Rust core. Python + TypeScript
bindings generated from the same source of truth. Apache 2.0. That's it.

The latency half is measured: **p50 19.2–22.4 ms / p99 23.4–24.4 ms**
engine-side at 100k docs/scope on single-shard Moon v0.8.5 (Apple M4 Pro,
graph OFF, rerank OFF, k=30) —
[`capacity.md`](https://github.com/pilotspace/lunaris/blob/main/docs/operations/capacity.md).
**We do not claim "millions":** the 1k → 100k trend (0.7 ms → ~20 ms p50)
says a million-fact scope would not meet 25 ms p50 on that hardware, and no
run at that size exists.

You feed it raw observations — chat turns, documents, tool outputs — as
`Episode`s. It chunks, embeds, and (optionally) extracts entities,
relations, and facts using a small local LLM, then stores everything in a
bi-temporal MVCC store backed by **Moon** (a high-performance
Redis-compatible substrate) — and, since 0.7.0, only Moon: the Postgres and
SQLite backends were removed. Agents query it
through a composable retrieval DSL that fuses semantic search, graph
traversal, and BM25 keyword lookup, with an optional cross-encoder rerank
pass on top.

## The three moats

Three properties define what Lunaris **is**. Every commit is reviewed
against them; any feature that weakens any of the three is rejected.

| Moat | What it means | Where enforced |
|---|---|---|
| **Sub-25 ms p50 recall** | No LLM on the recall hot path. Measured p50 19.2–22.4 ms / p99 23.4–24.4 ms at 100k documents per scope, graph OFF, rerank OFF. The opt-in cross-encoder rerank is a **quality** stage, not a latency-class stage — it measures **p50 1301.3 ms** at `top_in=60` and voids this contract when enabled. | `scripts/bench/perf/recall_latency.sh all` — a **manual, local** ~10-minute live-Moon gate. **Not CI-enforced:** `perf-gates.yml` is opt-in behind a `perf-bench` label, is not a required check, and is red on main ([`capacity.md`](https://github.com/pilotspace/lunaris/blob/main/docs/operations/capacity.md)) |
| **Single `atomic_write` per ingest** | All-or-nothing commit across vector, KV, BM25, audit, and queue. Fan-out architectures (Mem0, Zep) can't make this guarantee. | `crates/lunaris-ingest/tests/ingest_pipeline.rs::single_atomic_write_call` + CI grep gate |
| **Bi-temporal MVCC + HLC** | `BiTemporal { valid, sys }` on every primitive; `forget` and supersession close intervals instead of destroying rows. "What did the agent know at time T" is a query on the **search and graph** lanes (`FT.SEARCH AS_OF`, `GRAPH.QUERY VALID_AT`) — a historical **KV** read has no version chain on Moon and `read_as_of` **refuses** past the 1-hour live window rather than answering with today's data. | Required field on `Episode`, `Chunk`, `Entity`, `Fact`, `Relation`, `Community` (`crates/lunaris-core/src/bitemporal.rs`); the refusal is pinned by `crates/lunaris-conformance/tests/run_as_of_moon_gap.rs` |

If everything else fails, that performance + correctness contract must
hold — it's what differentiates Lunaris from Mem0, Zep, and Cognee.

A few more differentiators, with proof:

| Differentiator | What it gets you | Proof source |
|---|---|---|
| **Composable retrieval DSL** | `Vector::new("chunks", 30).and(Keyword::bm25("chunks", 30)).fuse_rrf(60).top(5)` is one typed expression. Hybrid search isn't a feature flag; it's an operator combinator. | `crates/lunaris-retrieve/src/builder.rs` |
| **Type-enforced multi-tenancy** | `Scope::new(s)?` validates against `[A-Za-z0-9_\-.]{1,128}`; the wire can't smuggle a different scope past `ScopedLunaris`. On Moon the scope is baked into the key, the FT index name and the graph name, so a cross-scope read has nothing to address. (Postgres RLS was the second boundary through 0.6.2; that backend was removed in 0.7.0.) | `crates/lunaris-core/src/scope.rs` ([RFC 0001](https://github.com/pilotspace/lunaris/blob/main/docs/rfcs/0001-scope-newtype.md)) |
| **Opt-in graph** | `Graph::anchored(entity_ids, hops)` is an operator. Off by default — your dev box doesn't load a graph extractor until you call `lunaris.graph_pipeline().enable()`. | `crates/lunaris-retrieve/src/operators/graph.rs` |
| **Remote-only verifier, no accidental cost** | The verifier resolves from `LUNARIS_VERIFY_PROVIDER` (anthropic/openai/gemini/minimax/openai-compat) or a caller-supplied impl. With no provider configured the effective verifier is `NoopVerifier` (a `tracing::warn!` says so) — opt in deliberately, no local model to stage. | `crates/lunaris-verify/src/lib.rs` |
| **One substrate, not three** | Moon holds the vector index, the graph, the keyword index, and the queue. No vector DB + graph DB + relational DB to operate. | [Choosing a Backend](../operations/backends.md) |

## When Lunaris is the answer

Pick Lunaris when you can say **yes** to most of these:

- My agent's recall p50 is on the hot path of user experience —
  300 ms feels slow.
- I want a single substrate (Moon) instead of running a
  vector DB + graph DB + relational DB.
- I need bi-temporal queries: "what did the agent believe at time T?"
  (bi-temporal *writes* always; as-of *reads* on the search and graph lanes
  only — a historical KV read is refused on Moon, so if you need to hydrate a
  row *as it was*, Lunaris v0 is not the tool)
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
| Recall latency contract | sub-25 ms p50, **measured** at 100k docs/scope (manual bench — *not* CI-enforced) | best-effort | best-effort | best-effort |
| Atomic ingest | single `atomic_write`, all-or-nothing | fan-out writes | fan-out writes | task pipeline |
| Bi-temporal | yes at the storage layer (`valid` + `sys`); as-of *reads* on the search + graph lanes, **not** on KV hydrate | no | yes | partial |
| Substrate count | 1 (Moon; Postgres and SQLite were removed in 0.7.0) | vector DB + store | vector DB + graph DB | configurable, multi |
| Hybrid retrieval | typed DSL: `vector.and(keyword).fuse_rrf().top()` | flag | flag | pipeline step |
| Graph | opt-in operator, off by default | Mem0g (Platform-only) | always-on | pipeline-driven |
| Multi-tenancy | `Scope` newtype + per-scope Moon keyspace, FT index and graph | `user_id` string | `session_id` | namespace |
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
