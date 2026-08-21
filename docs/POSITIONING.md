# Lunaris positioning — one page

This doc answers "should I use Lunaris?" before you read any other
docs. It's the page a maintainer points at when an evaluator asks
"how does this compare to X?" The migration docs (Mem0, Zep, Cognee)
go deeper; this page is the elevator pitch + the honest exclusions.

## The one-line claim

**Sub-25 ms recall at 100,000 documents per scope, with provable
atomicity and an opt-in graph.** Embedded Rust core. Python + TS
bindings. Apache 2.0. That's it.

The latency half of that claim is measured, not projected: **p50
19.2–22.4 ms / p99 23.4–24.4 ms** engine-side at 100k docs/scope on
single-shard Moon v0.8.5 (Apple M4 Pro, graph OFF, rerank OFF, k=30) —
[`docs/operations/capacity.md`](operations/capacity.md). **We do not claim
"millions."** The 1k → 100k trend (0.7 ms → ~20 ms p50) says a
million-fact scope would not meet 25 ms p50 on that hardware, and no run
at that size exists.

## What "Lunaris is the answer" looks like

Pick Lunaris when you can say **yes** to most of these:

- My agent's recall p50 is on the hot path of user experience —
  300 ms feels slow.
- I want a single substrate (Moon) instead of running a vector DB +
  graph DB + relational DB.
- I need bi-temporal queries: "what did the agent believe at time T?"
  (bi-temporal *writes* always; as-of *reads* on the search and graph
  lanes. A historical **KV** read is refused with an explicit 501 rather
  than silently answered with present-time data — Moon has no version
  chain, and the Postgres/SQLite backends that did were removed in 0.7.0.
  See
  [ARCHITECTURE.md § Honest limits](ARCHITECTURE.md#honest-limits-read-before-quoting-the-table-above).)
- I need multi-tenant isolation that the type system enforces, not
  just a `user_id` string the caller could swap.
- I want a composable retrieval DSL where vector + keyword + graph
  fuse in one typed expression, not three API calls glued together.
- My stack is Rust, or Python with a Rust binary in the build chain
  is acceptable, or TypeScript with a NAPI binding is acceptable.
- Apache 2.0 + open source matters; I want to read the substrate
  code, not depend on a hosted service.

## What "Lunaris is the wrong answer" looks like

Pick a different tool when you can say **yes** to any of these:

- I need a hosted SaaS with zero infra ownership. Lunaris does not
  ship a managed service (v0.4 milestone). Use **Zep Cloud** or
  **Mem0**.
- My agent's recall latency budget is 500+ ms anyway. The 25 ms
  contract isn't free to operate; if you don't need it, pick the
  hosted option.
- My stack is pure Python and adding a Rust binary to the deploy
  pipeline is more friction than it's worth. Use **Mem0**.
- My custom domain logic lives in a complex ingest pipeline with
  many plug-in Tasks. Use **Cognee**; its pipeline model is the
  strength.
- I want a "memory + chat agent in 5 minutes" with zero substrate
  knowledge. Use **Mem0** — it's optimized for that.
- I don't want to operate Moon. It is the only backend as of 0.7.0 —
  the Postgres and SQLite adapters were removed. `StoragePort` is still
  a public trait, so implementing it for LanceDB / Qdrant / Weaviate /
  Neo4j is an open extension point, but nothing ships one; if you need
  that on day one, pick a tool with a first-class adapter.

## The moat (what differentiates Lunaris, with proof)

| Differentiator                          | What it gets you                                              | Proof source                                                      |
|------------------------------------------|---------------------------------------------------------------|--------------------------------------------------------------------|
| **Single `atomic_write` per ingest**    | All-or-nothing commit across vector + KV + BM25 + audit + queue. Fan-out architectures (Mem0, Zep) can't make this guarantee. | `crates/lunaris-ingest/tests/ingest_pipeline.rs::single_atomic_write_call` + CI grep gate |
| **Bi-temporal at the storage layer**    | `.as_of(ts)` pushes the temporal cut into the query, not Python post-filter. 1M-fact corpora pay no Python tax. | `crates/lunaris/src/recall.rs` + `docs/benchmarks/v0.2.x/README.md` |
| **Composable retrieval DSL**            | `.vector().and(.keyword()).fuse_rrf(60).top(5)` is one typed expression. Hybrid search isn't a feature flag; it's an operator combinator. | `crates/lunaris-retrieve/src/builder.rs` |
| **Type-enforced multi-tenancy**         | `Scope::new(s)?` validates against `[A-Za-z0-9_\-.]{1,128}` regex; the wire can't smuggle a different scope past `ScopedLunaris`. RLS enforces it again on the database boundary. | `crates/lunaris-core/src/scope.rs` + `docs/migration/0.1-to-0.2.md` §6.2 |
| **Opt-in graph**                        | `Graph::anchored(entity_ids, hops)` is an operator. Off by default — your dev box doesn't load a graph extractor until you call `.with_graph_pipeline(true)`. | `crates/lunaris-retrieve/src/operators/graph.rs` |
| **Pluggable verifier, laptop-friendly** | Remote-only verifier (llama.cpp-only cutover): point `LUNARIS_VERIFY_PROVIDER` at any cloud or OpenAI-compatible local server — zero local LLM weights, zero RAM floor. | `crates/lunaris-verify/src/cloud_api.rs` + `docs/decisions/2026-07-10-llamacpp-only-cutover.md` |

## Migration paths

If you're already running an agent-memory tool, the migration docs
walk through ingest, recall, time-travel, forget — code-side, with
honest "stay on $incumbent if…" criteria.

- **[`docs/MIGRATING-FROM-MEM0.md`](MIGRATING-FROM-MEM0.md)** — no
  bi-temporal upgrade in Mem0, so the conversation is the bi-temporal
  upgrade.
- **[`docs/MIGRATING-FROM-ZEP.md`](MIGRATING-FROM-ZEP.md)** — Zep
  already has bi-temporal, so the conversation is latency + substrate
  simplification (1 vs 2 services).
- **[`docs/MIGRATING-FROM-COGNEE.md`](MIGRATING-FROM-COGNEE.md)** —
  Cognee is pipeline-oriented; Lunaris is operator-oriented. The
  conversation is "where does your domain logic live — ingest time
  or recall time?"

## Where to go next

1. **You want to try it now.** → [`examples/quickstart-rs/`](../examples/quickstart-rs/)
   (or `quickstart-py/`, `quickstart-ts/`). Docker-compose +
   working agent in 10 minutes.
2. **You want to evaluate.** Read the moat table above + the
   migration doc for your current tool, then run
   `examples/quickstart-py/` against your real corpus.
3. **You want to contribute.** Read [`CLAUDE.md`](../CLAUDE.md) for
   engineering constraints + [`docs/rfcs/`](rfcs/) for in-flight
   design decisions.
4. **You want to publish a v0.2.x cut.** Read
   [`docs/RELEASE.md`](RELEASE.md), then run `make release-preflight`.

## What's still on the runway

v0.2.x is an OSS-publish milestone, not the end state. The next
two milestones:

- **v0.3 self-hosted** — Docker / Helm, SLOs, design partners. The
  hosted-substrate experience for teams that want Lunaris but don't
  want to operate Moon themselves.
- **Ecosystem (shipped)** — LangGraph / CrewAI / Letta adapters via the
  `lunaris-integrations` package (`pip install lunaris-integrations[langgraph]`,
  `[crewai]`, `[letta]`). The "drop Lunaris into your existing agent framework"
  experience: LangGraph and CrewAI are drop-in store / storage classes; Letta
  ships as a client-backed connector shim + recipe (its archival store is
  server-side). The MCP server remains the universal shim for frameworks
  without a dedicated adapter.

See [`README.md#status`](../README.md#status) for the current
milestone state table.
