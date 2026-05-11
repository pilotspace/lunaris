# Lunaris

**Sub-25 ms recall over millions of bi-temporal facts, with provable
atomicity and a graph that's opt-in.**

A production-grade agent-memory engine in Rust, with first-class
Python and TypeScript SDKs. Raw observations in; structured,
bi-temporal facts out. Backed by Postgres (default) or Moon (the
high-performance Redis-compatible substrate).

```rust
use lunaris::{EpisodeBuilder, Lunaris, Scope};

let lunaris = Lunaris::open("postgres://lunaris@localhost/lunaris").await?;
let scope   = Scope::new("acme.agent-1")?;
let scoped  = lunaris.scoped(scope);

let ep = EpisodeBuilder::new("user-msg", "Alice loves chocolate.");
let lsn = scoped.ingest(ep).await?;
```

## Why Lunaris

Three properties define what Lunaris IS. Every commit is reviewed
against them; any feature that weakens any of the three is rejected.

| Moat | What it means | Where enforced |
|---|---|---|
| **Sub-25 ms p50 recall** | No LLM on the recall hot path. Cross-encoder rerankers stay sub-30 ms. | `cargo bench --bench eval05` |
| **Single `atomic_write` per ingest** | All-or-nothing commit across vector, KV, BM25, queue. Fan-out architectures (Mem0, Zep) can't make this guarantee. | `tests/ingest_pipeline.rs::single_atomic_write_call` + CI grep gate |
| **Bi-temporal MVCC + HLC** | `BiTemporal { valid, sys }` on every primitive. "What did the agent know at time T" is a query, not a rebuild. | Required field on `Episode`, `Chunk`, `Entity`, `Fact`, `Relation`, `Community` |

## Install

```bash
# Rust
cargo add lunaris

# Python
pip install lunaris

# TypeScript
npm i lunaris
```

Default features assume Postgres + Ollama running locally. For the
laptop-floor build (~540 MB RAM, no external Ollama), see RFC 0006
and the `verify-small` feature in `crates/lunaris-verify/Cargo.toml`.

## 10-minute quickstart

```bash
git clone https://github.com/lunaris-dev/lunaris && cd lunaris
cd examples/quickstart-rs
docker compose up -d                          # Postgres + pgvector + pgmq + AGE
ollama serve & ollama pull nomic-embed-text   # tiny embedder
export LUNARIS_PG_URL="postgres://lunaris:lunaris@localhost:5432/lunaris"
cargo run --release
```

Expected output:

```
quickstart: opening lunaris handle at postgres://...
quickstart: ingested episode at lsn=Lsn(1) under scope `quickstart`
```

Full walkthrough: [`examples/quickstart-rs/README.md`](examples/quickstart-rs/README.md).

## Architecture at a glance

```
┌─────────────────────────┐
│  Recipes (opinionated)  │  10 vertical wrappers
└────────────┬────────────┘
┌────────────┴────────────┐
│ Primitives (composable) │  MessageStream · DocumentCorpus
│                         │  TemporalQuery · WorkingMemory
└────────────┬────────────┘
┌────────────┴────────────┐
│   Retrieve DSL (fused)  │  Vector · Keyword · Graph
│                         │  RRF fusion · Rerank
└────────────┬────────────┘
┌────────────┴────────────┐
│ Storage (MVCC, bi-temp) │  Moon (Redis-compat) · Postgres
└─────────────────────────┘
```

## Multi-agent isolation

Every Lunaris operation is partitioned by `Scope` — a validated
newtype enforced at compile time and at the storage boundary
(Postgres RLS with `WITH CHECK`, per-scope Moon keyspaces).
Cross-scope reads are a type error. See [RFC 0001](docs/rfcs/0001-scope-newtype.md).

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
| **v0.2.1** | Shipped — multi-agent partitioning, full v0.2 release-gate close-out |
| **v0.2.x OSS** | In progress — laptop-floor verifier (RFC 0006), 10-min quickstart, PyPI/npm publish |
| **v0.3 Self-hosted** | Planning — Docker/Helm, SLOs, design partners |
| **v0.4 Ecosystem** | Future — LangGraph/CrewAI/Letta adapters |

See [`CHANGELOG.md`](CHANGELOG.md) for the full history.

## Coming from another agent-memory tool?

- **[`docs/MIGRATING-FROM-MEM0.md`](docs/MIGRATING-FROM-MEM0.md)** —
  Mem0 → Lunaris with code-side comparisons (ingest, recall,
  time-travel, forget), a 5-step incremental migration plan, and
  honest "stay on Mem0 if…" criteria.

## For contributors

- **[`CLAUDE.md`](CLAUDE.md)** — project-wide engineering constraints (Rust edition, MSRV, file size, lock discipline).
- **[`docs/rfcs/`](docs/rfcs/)** — design contracts. 0001 (Scope) shipped; 0004 / 0006 / 0007 in Draft.
- **[`.planning/`](https://github.com/lunaris-dev/lunaris-planning)** — milestones, requirements, roadmap, decision log (submodule).
- **[`docs/migration/0.1-to-0.2.md`](docs/migration/0.1-to-0.2.md)** — upgrade guide.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
