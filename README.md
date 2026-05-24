# Lunaris

**Sub-25 ms recall over millions of bi-temporal facts, with provable
atomicity and a graph that's opt-in.**

A production-grade agent-memory engine in Rust, with first-class
Python and TypeScript SDKs. Raw observations in; structured,
bi-temporal facts out. Backed by Postgres (default) or Moon (the
high-performance Redis-compatible substrate).

> **Documentation:** the full guide lives in the **[Lunaris Book](docs/book/)**
> (`mdbook serve docs/book` locally, or the published site at lunaris.dev once
> live) — getting started, the retrieval-DSL guide, the cookbook, and the
> complete [configuration reference](docs/book/src/reference/configuration.md).
>
> **First time here?** [`docs/POSITIONING.md`](docs/POSITIONING.md) is
> the one-page elevator pitch + honest "use a different tool when…"
> criteria. Read that before evaluating.

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
| **Sub-25 ms p50 recall** | No LLM on the recall hot path. Cross-encoder rerankers stay sub-30 ms. | `cargo bench --bench recall_hot_path` |
| **Single `atomic_write` per ingest** | All-or-nothing commit across vector, KV, BM25, queue. Fan-out architectures (Mem0, Zep) can't make this guarantee. | `tests/ingest_pipeline.rs::single_atomic_write_call` + CI grep gate |
| **Bi-temporal MVCC + HLC** | `BiTemporal { valid, sys }` on every primitive. "What did the agent know at time T" is a query, not a rebuild. | Required field on `Episode`, `Chunk`, `Entity`, `Fact`, `Relation`, `Community` |

## Install

```bash
# Rust — the crate is published as `lunaris-memory`; import it as `lunaris`
cargo add lunaris-memory --rename lunaris

# Python
pip install lunaris

# TypeScript
npm i lunaris
```

v0.4 ships candle-native `granite-embedding-311m-multilingual-r2` (embedder,
FP16, 768-d) + `bge-reranker-v2-m3` (cross-encoder reranker, FP32). Stage the
weights once via `huggingface-cli download` — see
[`docs/migration/0.3-to-0.4-native-default.md`](docs/migration/0.3-to-0.4-native-default.md).
Operators on air-gapped networks with an existing Ollama deployment can opt
into the HTTP escape hatch via `--features embed-remote` +
`LUNARIS_EMBEDDER_OLLAMA_URL`. For Q4_K_M GGUF (RSS-constrained
deployments), build with `--features embedder-gguf,reranker-gguf` and set
`LUNARIS_EMBEDDER_GGUF` / `LUNARIS_RERANKER_GGUF`. Full matrix in the
[configuration reference](docs/book/src/reference/configuration.md).

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
| **v0.3.0** | In progress on `main` — `Lunaris::list_scopes`, W1/W2 wave (schema_gate, code_feature_card recipe, FT.INVALIDATE_RANGE fan-out), vendor/moon bump to Moon main (version_token + FT.INVALIDATE_RANGE) |
| **v0.3 Self-hosted** | Planning — Docker/Helm, SLOs, design partners |
| **v0.4 Ecosystem** | Future — LangGraph/CrewAI/Letta adapters |

See [`CHANGELOG.md`](CHANGELOG.md) for the full history.

## Coming from another agent-memory tool?

- **[`docs/MIGRATING-FROM-MEM0.md`](docs/MIGRATING-FROM-MEM0.md)** —
  Mem0 → Lunaris with code-side comparisons (ingest, recall,
  time-travel, forget), a 5-step incremental migration plan, and
  honest "stay on Mem0 if…" criteria.
- **[`docs/MIGRATING-FROM-ZEP.md`](docs/MIGRATING-FROM-ZEP.md)** —
  Zep → Lunaris with the parallel comparison. Differs from Mem0
  because Zep already has bi-temporal facts — the focus is latency
  + substrate simplification, not the bi-temporal upgrade.
- **[`docs/MIGRATING-FROM-COGNEE.md`](docs/MIGRATING-FROM-COGNEE.md)** —
  Cognee → Lunaris. Different conversation again: pipeline-vs-DSL
  tradeoff. If your custom logic lives at ingest time, Cognee's
  Task model maps cleaner; if it lives at recall time, Lunaris's
  composable operator DSL is the simpler model.

## For contributors

- **[`CLAUDE.md`](CLAUDE.md)** — project-wide engineering constraints (Rust edition, MSRV, file size, lock discipline).
- **[`docs/rfcs/`](docs/rfcs/)** — design contracts. 0001 (Scope) shipped; 0004 / 0006 / 0007 in Draft.
- **[`.planning/`](https://github.com/lunaris-dev/lunaris-planning)** — milestones, requirements, roadmap, decision log (submodule).
- **[`docs/migration/0.1-to-0.2.md`](docs/migration/0.1-to-0.2.md)** — upgrade guide.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
