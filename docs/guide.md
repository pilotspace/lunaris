# Lunaris User Guide

Status: alpha tracking v0.1.1 (Rust crate `lunaris`, `pip install lunaris`, `npm i @pilotspace/lunaris`). Complements `docs/protocol/memoryprotocol-0.1.md` (HTTP wire spec). If a claim here disagrees with the Rust source, the source wins — every symbol below has a `path:line` cross-reference.

## What is Lunaris?

Lunaris is a multi-language agent memory engine — Rust-first, with PyO3 + napi-rs bindings generated from the same source of truth. You feed it raw observations (markdown documents, chat turns, tool outputs) as `Episode`s; it chunks, embeds, and (optionally) extracts entities/relations/facts via a local LLM, then stores everything in a bi-temporal MVCC key-value + vector + graph substrate. You query it through a composable retrieval **DSL** — a *Domain-Specific Language*, i.e. a small purpose-built query API where you chain operators into a plan rather than glue together raw calls — that fuses vector search, BM25 keyword lookup, graph traversal, and RAPTOR hierarchical tree retrieval, with a cross-encoder rerank pass on top.

One backend ships: **Moon** (Redis-compatible, with `FT.*` native vector + BM25 + RRF, `GRAPH.QUERY`, and a native MQ). The Postgres and SQLite backends were deleted in 0.7.0; `moon://host:port` is the only URL scheme `Lunaris::open` accepts.

### Mental model

```text
          Episode
             |
             v
      +-------------+        (optional)         +----------------+
      |   ingest    | -- graph pipeline ON --> |  extract       |
      |  (chunk,    |                          |  entities +    |
      |   embed,    |                          |  relations +   |
      |   atomic    |                          |  facts         |
      |   write)    | <---- validator --------+----------------+
      +------+------+
             |                                            ^
             v                                            |
      +-------------+                             +--------------+
      |   storage   | <-- MVCC supersede -------> | forget       |
      | (KV/Vector/ |       (soft/hard)           | (GDPR/audit) |
      |   Graph/    |                             +--------------+
      |   Queue)    |
      +------+------+
             |
   recall    v        rerank
   builder -+-> [vector] ----\                       queue
            +-> [bm25 ]  ---- fuse_rrf --> rerank --> Hit[]
            +-> [graph] ----/                          |
                                                       v
                                               +-------+-------+
                                               | __lunaris_    |
                                               |  verify__     |  <-- verifier worker (default OFF)
                                               | __lunaris_    |
                                               |  consolidate__|  <-- consolidator worker (default OFF)
                                               | __lunaris_    |
                                               |  audit__      |  <-- audit sink
                                               +---------------+
```

`ingest` and `forget` each commit in a **single** `StoragePort::atomic_write` call. `recall` is a read-only DSL that composes retrievers into a plan, executes once, and returns `Vec<Hit>`.

---

## 1. Install & open a handle

### Problem

You have a Rust program and want to talk to Lunaris.

### When to use it

Every program begins here. You pick between the raw storage port (`lunaris::open`) and the high-level handle (`lunaris::Lunaris::open`). Use the handle unless you are writing a conformance test that needs to sidestep the ingest/recall pipelines.

### Code

`Cargo.toml`:

```sh
cargo add lunaris-memory --rename lunaris
cargo add tokio --features macros,rt-multi-thread
```

The umbrella crate is published as **`lunaris-memory`** (the bare `lunaris` name
on crates.io is an unrelated project); the `--rename` keeps the import path
`use lunaris::…`, so the snippets below are unaffected:

```toml
[dependencies]
lunaris = { package = "lunaris-memory", version = "0.6" }
tokio   = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Features (see the `[features]` section of `crates/lunaris/Cargo.toml`):

- `default = ["llamacpp"]` — the llama.cpp inference runtime (llama.cpp-only cutover, ADR 2026-07-10): embedder `granite-embedding-311m-multilingual-r2` Q4_K_M GGUF (768-d) + reranker `bge-reranker-v2-m3` Q5_K_M GGUF. GGUFs load from `~/.lunaris/models/` (override: `LUNARIS_EMBEDDER_GGUF` / `LUNARIS_RERANKER_GGUF`). Building with the default needs cmake + a C++ toolchain; `default-features = false` gives the Tier-0 no-inference build (NoopEmbedder/NoopReranker, pure Rust).
- `metal` / `cuda` / `vulkan` — per-target GPU offload for the llama.cpp runtime (CPU is the default device).
- `ollama` — HTTP-backed extractor + verifier pointing at `http://localhost:11434`.
- `cloud-api` — remote extractor + verifier backends (Anthropic / OpenAI / Gemini / MiniMax / any OpenAI-compatible URL), resolved from `LUNARIS_EXTRACT_PROVIDER` / `LUNARIS_VERIFY_PROVIDER`. There is no in-process LLM backend — see [the 0.5→0.6 migration note](migration/0.5-to-0.6-llamacpp-only.md).
- `embed-remote` — air-gap escape hatch: route the embedder through an existing Ollama instance via `LUNARIS_EMBEDDER_OLLAMA_URL` (operator-only, resolved after the llama.cpp step).
- `moon-it`, `pg-it` — gate live-backend integration tests.

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    // High-level handle — this is what application code uses.
    let lunaris = Arc::new(lunaris::Lunaris::open("moon://localhost:6380").await?);
    println!("{lunaris:?}");
    Ok(())
}
# Ok(()) }
```

If all you need is the raw `Arc<dyn StoragePort>` (Plan 5 conformance harness, low-level tests):

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
let storage = lunaris::open("moon://localhost:6380").await?;
// storage is Arc<dyn StoragePort>. No ingest/recall surface; you drive atomic_write, read_as_of, etc.
# Ok(()) }
```

URL schemes are matched at `crates/lunaris/src/open.rs` and `crates/lunaris/src/handle.rs`. Only `moon://` is accepted; every other scheme — including the retired `postgres://` / `postgresql://` / `sqlite://` / `memory://` — returns `LunarisError::Storage(StorageError::UnsupportedScheme(_))` carrying the migration link.

### Gotchas

- **GGUF staging**: the default embedder is `LlamaCppEmbedder` (`granite-embedding-311m-multilingual-r2` Q4_K_M GGUF, 768-d) loaded from `~/.lunaris/models/` (`resolve_embedder()` in `crates/lunaris/src/handle.rs`; override the path with `LUNARIS_EMBEDDER_GGUF`). On a missing GGUF `open` does **not** fail — it logs a `WARN` banner and falls back to a zero-vector `NoopEmbedder`, so the rest of the open path completes but **vector recall returns empty rows** until the GGUF is staged. Download it out-of-band (SHA-256s printed by `cargo run -p lunaris-bench --bin stage-models -- --help`) or swap to a different embedder with `with_embedder(...)` before first use.
- **Embedder / reranker ride the default `llamacpp` feature**: the `ollama` / `cloud-api` features only select the **extractor + verifier** LLM backends — see [the 0.5→0.6 llama.cpp-only migration note](migration/0.5-to-0.6-llamacpp-only.md).
- **Typos in the URL**: `mon://...` or `redis://...` yield `UnsupportedScheme`, not a connection error. Double-check the scheme when the error mentions a string you did not type.

---

## 2. Ingest your first episode

### Problem

You want data in the store, durably, with one transaction per call.

### When to use it

Every write begins with `Lunaris::ingest`. One call = one `atomic_write` on the backend, whether the graph pipeline is ON or OFF. The INGEST-04 single-call invariant is enforced inside `crates/lunaris/src/ingest.rs:72-127`.

### Code

The shape below paraphrases the canonical smoke test at `crates/lunaris/tests/ingest_smoke.rs:91-108`. In production you would use a real storage + embedder; here we use `StubEmbedder` so the test needs no model weights.

```rust,no_run
# use lunaris::Scope;
# fn my_storage() -> std::sync::Arc<dyn lunaris::StoragePort> { unimplemented!() }
use std::sync::Arc;

use lunaris::{Lunaris, Episode, HlcClock, StoragePort, Embedder};
use lunaris_core::StubEmbedder;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    // Production form — pulls the real storage + llama.cpp embedder from the URL:
    //   let lunaris = Lunaris::open("moon://localhost:6380").await?;
    //
    // Test form — matches tests/ingest_smoke.rs:91-108. Replace `my_storage()`
    // with any Arc<dyn StoragePort> (an in-memory recording fixture in tests,
    // or MoonStorage::connect directly in benches).
    let scope = Scope::new("acme-workspace")?;
    let storage: Arc<dyn StoragePort> = my_storage();
    let clock = HlcClock::new(0);
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let lunaris = Lunaris::with_parts(storage, embedder, clock.clone());

    // Episode::new fills id = Ulid::new(), bt = BiTemporal::now(clock),
    // metadata = {} — see crates/lunaris-core/src/primitives.rs:27.
    let ep = Episode::new(
        scope,
        "notes.md",
        "# Notes\nThe quick brown fox jumps over the lazy dog.",
        &lunaris.clock(),
    );

    let lsn = lunaris.ingest(ep).await?;
    println!("committed at lsn={lsn:?}");
    Ok(())
}
```

### Gotchas

- **One `atomic_write` per call, period.** `ingest` does not commit per chunk; it builds the full `Vec<WriteOp>` (episode KV + per-chunk KV + per-chunk vector upsert, plus graph/fact rows if the graph pipeline is ON) and hands it to the backend in a single call. See `crates/lunaris/src/ingest.rs:188-377`.
- **The returned `Lsn` is a replay cursor, not a primary key.** It tells you where the snapshot endpoint (`GET /v1/snapshot/{lsn}`) can resume. Callers dedupe on `Episode::id`, not on `Lsn`.
- **Ingest publishes to `__lunaris_consolidate__` after commit** (`crates/lunaris/src/ingest.rs:49-53` + `121`). Publish failure is logged but does not fail the ingest — the data is already durable. If the graph pipeline is ON, every `NeedsReview` item also publishes to `__lunaris_verify__` (`crates/lunaris/src/ingest.rs:48`, `443-457`).

---

## 3. Recall: the DSL in four steps

### Problem

You want hits back without writing your own retriever.

### When to use it

Every read beyond a single-key fetch. The DSL composes four leaf operators — `Vector`, `Keyword`, `Graph`, `Tree` (RAPTOR hierarchical) — plus fusion/rerank/modifier wrappers, and executes the plan in one pass. The canonical shape lives at `crates/lunaris/src/recall.rs:46-72`; the canonical test is `crates/lunaris/tests/recall_smoke.rs:200-217`.

### Code

#### Step 1 — pure vector

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris::{Query, Vector};

let hits = lunaris
    .recall()
    .with_root(Vector::new("chunks", 30).top(5))
    .execute(Query::text("brown fox"))
    .await?;
# Ok(()) }
```

`Vector::new("chunks", 30)` asks the backend for the top 30 chunks by vector similarity; `.top(5)` caps the final result. The `chunks` index is one of four whitelisted names (`chunks | entities | facts | communities`).

#### Step 2 — add BM25

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris::{Keyword, Query, Vector};

let hits = lunaris
    .recall()
    .with_root(
        Vector::new("chunks", 30)
            .and(Keyword::bm25("chunks", 30))
            .top(5),
    )
    .execute(Query::text("brown fox"))
    .await?;
# Ok(()) }
```

`Keyword::bm25("chunks", 30)` is defined at `crates/lunaris-retrieve/src/operators/keyword.rs:25`. `.and(...)` is the combinator from `combinators.rs:36` — both retrievers run and their results flow into the next operator.

#### Step 3 — fuse with reciprocal rank

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
let hits = lunaris
    .recall()
    .with_root(
        Vector::new("chunks", 30)
            .and(Keyword::bm25("chunks", 30))
            .fuse_rrf(60)
            .top(5),
    )
    .execute(Query::text("brown fox"))
    .await?;
# Ok(()) }
```

`fuse_rrf` detects the (Vector + Keyword(BM25)) shape on the same index and dispatches to Moon's native `text().hybrid_search` — **one** round trip instead of two (`crates/lunaris-retrieve/src/operators/fuse.rs`, governed by `StorageCapabilities::native_rrf` in `crates/lunaris-core/src/storage.rs`). A backend that does not declare the capability fuses client-side in the same operator; the API is identical either way.

#### Step 4 — rerank

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
let hits = lunaris
    .recall()
    .with_root(
        Vector::new("chunks", 30)
            .and(Keyword::bm25("chunks", 30))
            .fuse_rrf(60)
            .top(30),
    )
    .rerank(lunaris.reranker())
    .top(5)
    .execute(Query::text("brown fox"))
    .await?;
# Ok(()) }
```

`RetrievalBuilder::rerank` and `::top` are both builder-level (`crates/lunaris-retrieve/src/builder.rs:162-171`). The reranker is cross-encoder bge-reranker-v2-m3 by default; when weights are missing, `Lunaris::open` installs `NoopReranker` and `handle.reranker()` still returns a working `Arc<dyn Reranker>` that passes scores through unchanged (`crates/lunaris/src/handle.rs:494-515`).

### Gotchas

- **`filter_str` returns a `Result`.** Parse errors come back as `FilterParseError` — unwrap with `?` in your application code; never `.unwrap()` on user input. See `crates/lunaris-retrieve/src/builder.rs:136`.
- **Empty hits are usually a filter problem.** The v0 string-DSL only parses `field LIKE 'prefix%'` predicates (`crates/lunaris-retrieve/src/operators/modifiers.rs:80`). If you over-constrain, drop the filter and re-run.
- **`RetrievalBuilder` is sync; `execute` is the only `.await`.** Side-effectful wiring (setting `with_root`, attaching filters, enabling degraded mode) happens before any async work, which keeps the builder `Send` without future-boxing.

---

## 4. Graph-aware recall

### Problem

You want to follow relations out from a known entity ("everything we know about Alice that also matches this query").

### When to use it

After the graph pipeline is enabled and you've ingested Episodes — chunks extracted entities + relations, which landed as `GraphNode` + `GraphEdge` rows. Now `Graph::anchored(entity_ids, hops)` traverses the same index.

### Code

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris::{EntityId, Graph, Keyword, Query, Vector};

// Flip the pipeline ON. Default is OFF (blueprint §5.2).
// Env alternative: LUNARIS_GRAPH_ENABLED=1 (handle reads this at open time).
lunaris.graph_pipeline().enable();

// Ingest episodes here so the extractor writes GraphNodes + GraphEdges.
// (See Section 2 — same ingest call; the branch lights up automatically.)

// The anchor: a content-hash EntityId derived deterministically from
// name + type (crates/lunaris-extract/src/types.rs:29).
let alice = EntityId::from_name_and_type("Alice", "Person");

let hits = lunaris
    .recall()
    .with_root(
        Vector::new("chunks", 30)
            .and(Graph::anchored(vec![(alice, 1.0)], 2))
            .fuse_rrf(60)
            .top(30),
    )
    .rerank(lunaris.reranker())
    .top(5)
    .execute(Query::text("Tell me about Alice"))
    .await?;
# Ok(()) }
```

This mirrors the canonical compose example in the `recall()` doc comment (`crates/lunaris/src/recall.rs:62-72`).

### Gotchas

- **Graph is default OFF.** Blueprint §5.2 + `crates/lunaris/src/handle.rs:124-130`. Toggling at runtime via `handle.graph_pipeline().enable()` is idempotent; `LUNARIS_GRAPH_ENABLED=1` seeds the initial state at open time.
- **Hops are capped.** `DEFAULT_GRAPH_HOPS = 2`, `MAX_GRAPH_HOPS = 5` at `crates/lunaris-retrieve/src/operators/graph.rs:54-59`. Asking for more gets clamped.
- **The extractor is required for graph ingest.** Extraction is remote-only (llama.cpp-only cutover): with `LUNARIS_EXTRACT_PROVIDER` unset the handle uses `NoopExtractor` and `graph_pipeline().enable()` is a no-op — you will get zero `GraphNode`s written. Fix by setting `LUNARIS_EXTRACT_PROVIDER` (anthropic|openai|gemini|minimax|openai-compat, see the [0.5→0.6 migration note](migration/0.5-to-0.6-llamacpp-only.md)) or via `handle.with_extractor(Arc::new(OllamaExtractor::new(...)?))` (`ollama` feature).
- **Graph traversal uses `Vector::and(Graph::anchored(...))`.** When a `Graph` branch is present, `fuse_rrf` always uses client-side fusion (`crates/lunaris-retrieve/src/fusion.rs`) — the Moon-native one-trip path only fires for Vector+Keyword(BM25) on the same index.

---

## 5. Forget

### Problem

Surgical deletes: GDPR right-to-be-forgotten, retention windows, session cleanup.

### When to use it

Whenever a primitive must stop being visible to future queries. Three variants; all go through one `Lunaris::forget` entry point (`crates/lunaris/src/forget.rs:206-298`). Every successful call emits one `__lunaris_audit__` event (OPS-04; `crates/lunaris/src/forget.rs:289-294`).

### Code

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris::{ForgetTarget, ScopeSpec};

// Soft delete — MVCC: stamps bt.sys_to, prior reads via as_of still work.
let scope = ForgetTarget::Scope(ScopeSpec::BySource(
    "helios:fs/session-42/".into(),
));
let receipt = lunaris.forget(scope.clone()).await?;
assert!(!receipt.preview);
assert!(receipt.rows_written > 0);

// Dry-run preview — no atomic_write; receipt shape is identical but preview=true.
let preview = lunaris.forget(scope.clone().dry_run()).await?;
assert!(preview.preview);

// Hard delete — irreversible KvDelete fan-out, requires a ForgetConfirmation
// token minted from a prior dry_run receipt (D-21 two-step safety rail).
let token = lunaris.confirm_hard_forget(preview).await?;
let hard_receipt = lunaris
    .forget(scope.hard().with_token(token))
    .await?;
assert!(!hard_receipt.preview);
assert_eq!(hard_receipt.rows_written, 0);       // hard delete writes zero MVCC rows
assert!(hard_receipt.rows_deleted >= 1);        // one KvDelete per match
# Ok(()) }
```

The three target shapes live at `crates/lunaris/src/forget.rs:49-71`:

```rust,no_run
# use lunaris::{ForgetTarget, ScopeSpec};
# fn demo(target: ForgetTarget) {
// Both enums are `#[non_exhaustive]`, so the `_` arms are mandatory outside
// the crate: this listing goes red when a variant is RENAMED or REMOVED, but
// a newly ADDED variant slips past it silently.
match target {
    ForgetTarget::Id(_ulid)     => {} // OPS-01 — single-primitive purge
    ForgetTarget::Scope(spec)   => match spec {
        // OPS-02 — prefix / metadata / episode-id predicate
        ScopeSpec::BySource(_prefix)      => {} // prefix match on episode.source
        ScopeSpec::ByMetadata(_k, _v)     => {} // exact match on metadata[key]
        ScopeSpec::ByEpisode(_ulid)       => {} // exact match on episode.id
        _ => {}
    },
    ForgetTarget::Before(_hlc)  => {} // OPS-03 — AS_OF cutoff
    _ => {}
}
# }
```

### Gotchas

- **Hard delete without a token errors out.** You get `Err(LunarisError::Validate(ValidateError::ConfirmationRequired(_)))` — not a panic (`crates/lunaris/src/forget.rs:232-237`). `confirm_hard_forget` only accepts a `preview: true` receipt; replaying a non-preview receipt surfaces the same error (`crates/lunaris/src/forget.rs:307-317`).
- **One `atomic_write` per call, zero for dry-run** (`crates/lunaris/src/forget.rs:273-280`). The audit publish lands even on dry-run so ops has a complete trail of what almost happened.
- **Soft delete writes MVCC `sys_to` inside the payload bytes.** Backends derive persisted bitemporal from the payload, so a typed-only mutation is silently lost. `build_soft_delete_op` patches both the in-memory `BiTemporal` and the JSON payload (`crates/lunaris/src/forget.rs:468-502`) — this is the same cross-plan contract Plan 04-04's `apply_supersede` uses.

---

## 6. Background workers

### Problem

Slow-path quality checks (verifier) and memory consolidation (ACT-R) without blocking ingest or recall.

### When to use it

When your deployment has latency budget to spare AND you want provenance/quality signals landing in the audit stream. v0 ships both workers default-OFF (blueprint §5.1); v1 flips them on in production once queue-lag SLOs hold.

### Code

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
// Verifier — consumes __lunaris_verify__ messages emitted by the graph-ON
// ingest path and publishes VerifyDecision outcomes.
lunaris.verify_pipeline().enable();

// Consolidator — consumes __lunaris_consolidate__ (one message per ingest
// commit) and runs ACT-R activation updates / promotion / archival.
lunaris.consolidator_pipeline().enable();

// Degraded-recall check — reads the verifier queue depth once per call and
// sets Hit::degraded=true on every hit when depth > threshold.
let hits = lunaris
    .recall_with_degraded_check()
    .await?
    .with_root(lunaris::Vector::new("chunks", 30).top(5))
    .execute(lunaris::Query::text("status of x"))
    .await?;
for h in &hits {
    if h.degraded {
        tracing::warn!("verifier backlog — results may be stale");
    }
}
# Ok(()) }
```

Env seeds the initial state at open time:

- `LUNARIS_GRAPH_ENABLED=1|0` — toggle the graph pipeline (`crates/lunaris/src/graph_pipeline.rs`).
- `LUNARIS_RAPTOR_ENABLED=1|0` — toggle the RAPTOR community-tree write at
  ingest (`crates/lunaris-ingest/src/pipeline.rs`). **Default off.** Nothing on
  a default recall path reads the `communities` index — only the opt-in
  `.tree(..)` DSL operator does — so leaving it on pays an extra embedder
  round-trip and `2 × N` writes per ingest for nothing. Independent of
  `LUNARIS_GRAPH_ENABLED`.
- `LUNARIS_VERIFY_ENABLED=1|0` — toggle the verifier worker (`crates/lunaris/src/verify_pipeline.rs`).
- `LUNARIS_CONSOLIDATE_ENABLED=1|0` — toggle the consolidator worker (`crates/lunaris/src/consolidator_pipeline.rs`).
- `LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD=<u64>` — depth beyond which `recall_with_degraded_check` flags results. Default 1000 (`crates/lunaris/src/recall.rs:26-31`).

### Gotchas

- **Both workers default OFF.** Calling `.enable()` with no backend installed just runs the shipped `NoopVerifier` / `NoopConsolidator` — no crashes, but also no useful output. Wire a real backend (`handle.with_verifier(...)` / `handle.with_consolidator(...)`) before enabling if you want work done.
- **`recall_with_degraded_check` is best-effort.** If the backend's `queue_depth` returns `NotSupported`, recall falls through with `degraded=false` and logs at debug (`crates/lunaris/src/recall.rs:122-128`). Recall still returns hits.
- **Queue topics are hard-coded constants** — see `crates/lunaris/src/ingest.rs:48-53`. If you build your own worker off these topics, watch for consumer-group versioning: the shipped workers use `lunaris-verify-v0` / `lunaris-consolidate-v0` groups so a future message-schema bump can land on a fresh group.

---

## 7. HTTP

### Problem

You drive Lunaris from a non-Rust runtime (Python, TypeScript, Go, curl).

### When to use it

Helios will call it from Rust via the crate directly, but everyone else goes through `lunaris-server`. The wire protocol is MemoryProtocol v0.1 — see [`docs/protocol/memoryprotocol-0.1.md`](protocol/memoryprotocol-0.1.md) for the full JSON schemas, SSE contract, error taxonomy, and conformance gate. This section shows you how to run the server and hit each route with `curl`.

### Start the server

```bash
# One-time: create a bearer-token map. Every /v1/* request needs this.
cat > /tmp/lunaris-tokens.json <<'EOF'
{
  "dev-token-xxx": { "tenant": "dev", "scopes": ["ingest", "recall", "forget"] }
}
EOF

cargo run -p lunaris-server -- \
  --storage moon://localhost:6380 \
  --bind 0.0.0.0:8080 \
  --tokens-file /tmp/lunaris-tokens.json
```

Flags live in `crates/lunaris-server/src/config.rs:10-`; key knobs:

| Flag | Env | Default |
|------|-----|---------|
| `--bind` | `LUNARIS_BIND` | `0.0.0.0:8080` |
| `--storage` | `LUNARIS_STORAGE` | required |
| `--tokens-file` | `LUNARIS_TOKENS_FILE` | required |
| `--rate-per-second` | `LUNARIS_RATE_PER_SECOND` | `60` |
| `--shutdown-grace-secs` | — | see `--help` |

### Hit the routes

Every `/v1/*` call needs `Authorization: Bearer <token>` (`docs/protocol/memoryprotocol-0.1.md` §Authentication). `/healthz` and `/metrics` are unauthenticated probe surfaces.

```bash
# Health probe (no auth).
curl -s http://localhost:8080/healthz

# Ingest — scope "ingest".
curl -sX POST http://localhost:8080/v1/ingest \
  -H "Authorization: Bearer dev-token-xxx" \
  -H "Content-Type: application/json" \
  -d '{
        "source":  "notes.md",
        "content": "# Notes\nThe quick brown fox jumps over the lazy dog."
      }'
# -> { "lsn": { "wall_ms": ..., "counter": 0 }, "queue_lag_warn": false }

# Recall (JSON) — scope "recall".
curl -sX POST http://localhost:8080/v1/recall \
  -H "Authorization: Bearer dev-token-xxx" \
  -H "Content-Type: application/json" \
  -d '{ "query": "brown fox", "k": 5, "mode": "semantic" }'

# Recall (SSE stream) — set Accept: text/event-stream.
curl -sN -X POST http://localhost:8080/v1/recall \
  -H "Authorization: Bearer dev-token-xxx" \
  -H "Accept: text/event-stream" \
  -H "Content-Type: application/json" \
  -d '{ "query": "brown fox", "k": 5 }'

# Forget (soft) — scope "forget".
curl -sX POST http://localhost:8080/v1/forget \
  -H "Authorization: Bearer dev-token-xxx" \
  -H "Content-Type: application/json" \
  -d '{
        "target": { "Scope": { "BySource": "helios:fs/session-42/" } }
      }'

# Snapshot replay — scope "recall". NDJSON body, one WriteOp per line.
curl -s "http://localhost:8080/v1/snapshot/1713789012345.0" \
  -H "Authorization: Bearer dev-token-xxx"

# Metrics (Prometheus text — no auth).
curl -s http://localhost:8080/metrics
```

Route list is at `crates/lunaris-server/src/lib.rs:111-170`; request/response DTOs live at `crates/lunaris-server/src/dto.rs`.

### Gotchas

- **The plan's shorthand "`POST /ingest`" is wrong** — the real routes are under `/v1/` with `Authorization: Bearer` required. Anything that contradicts `docs/protocol/memoryprotocol-0.1.md` is a spec bug; raise it there.
- **Hard delete is two requests.** First `POST /v1/forget` with `dry_run: true`, read the `audit_lsn` out of the receipt, then repeat with `hard: true` and `confirmation_token: "<wall_ms>.<counter>"` formed from the prior audit LSN. See the protocol doc §Forget.
- **Logs default to pretty text on a TTY, JSON when `LUNARIS_ENV=production` or stdout is not a TTY** — `crates/lunaris/src/logging.rs:46` + CONTEXT.md D-26. Pipe through `jq` in production.
- **SIGTERM drains cleanly.** `--shutdown-grace-secs` controls how long the server waits for in-flight requests. The server exits non-zero on `Lunaris::open` failure with an actionable error on stderr (`crates/lunaris-server/src/main.rs:32-38`).
- **Per-tenant rate limit** (`60 rps / 120 burst` by default) bites before the handler runs — exceeding returns `429` with `Retry-After` (`docs/protocol/memoryprotocol-0.1.md` §Rate limiting).

---

## 8. Component swaps

### Problem

Tests need determinism; benches need specific backends; production wants to swap implementations without rebuilding the handle graph.

### When to use it

Every one of the five swap builders returns `Self`, so they chain. Apply before enabling the corresponding pipeline — otherwise the old component keeps running until the next `.enable()` cycle.

### Code

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use std::sync::Arc;

use lunaris::{NoopConsolidator, NoopExtractor, NoopReranker, NoopVerifier};

let lunaris = lunaris::Lunaris::open("moon://localhost:6380")
    .await?
    // Swap embedder — e.g., native granite-r2 → Ollama when the per-batch budget busts.
    // (handle.rs explains the motivating latency case; requires --features embed-remote.)
    .with_embedder(Arc::new(lunaris_embed_remote::OllamaEmbedder::new(Default::default())?))
    // Swap reranker — pin NoopReranker in tests for determinism.
    .with_reranker(Arc::new(NoopReranker))
    // Swap extractor — NoopExtractor when graph is off and you don't want the
    // model download (handle.rs:337-340).
    .with_extractor(Arc::new(NoopExtractor))
    // Wire real slow-path backends before flipping .enable() on the pipeline.
    .with_verifier(Arc::new(NoopVerifier))
    .with_consolidator(Arc::new(NoopConsolidator));
# Ok(()) }
```

The escape hatch constructor bypasses URL routing entirely. `tests/ingest_smoke.rs:96` uses it to wire an in-memory storage + `StubEmbedder`:

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
# use lunaris::{Embedder, HlcClock, StoragePort};
# fn my_recording_storage() -> Arc<dyn StoragePort> { unimplemented!() }
use lunaris_core::StubEmbedder;

let storage: Arc<dyn StoragePort> = my_recording_storage();
let embedder: Arc<dyn Embedder>  = Arc::new(StubEmbedder::new(768));
let clock                          = HlcClock::new(0);
let handle                         = Lunaris::with_parts(storage, embedder, clock);
# Ok(()) }
```

If your storage also implements `KeywordPort`, use `Lunaris::with_parts_keyword` instead so recall's BM25 path stays alive (`crates/lunaris/src/handle.rs:269`).

### Gotchas

- **Swaps return `Self`.** Builder style — chain them, or rebind `let lunaris = lunaris.with_extractor(...)`.
- **Apply before enabling.** `.enable()` captures a snapshot of the current component (`crates/lunaris/src/graph_pipeline.rs`, `verify_pipeline.rs`, `consolidator_pipeline.rs`). A swap afterwards propagates via `set_extractor` / `set_verifier` / `set_consolidator` and is preserved across toggle flips.
- **`with_parts` wires a `NoopReranker` by default.** Production callers that go through `Lunaris::open` get the real bge-reranker-v2-m3 when weights are present (`crates/lunaris/src/handle.rs:503-515`). Tests and benches that construct via `with_parts` must opt into rerank themselves.

---

## 9. Recipes

### Problem

You want opinionated shortcuts when you don't want to hand-compose the DSL, or you need a stable named surface across Rust + Python + TypeScript.

### When to use them

v0.1.1 ships three layers. Pick the lowest one that meets your need — thinner = more flexible; named = more discoverable and better documented in Py/TS.

1. **`lunaris.recall()` / `lunaris.ingest()`** (Sections 2–4) — the DSL. Full flexibility.
2. **Primitives** in `lunaris_recipes::{MessageStream, DocumentCorpus, TemporalQuery, WorkingMemory}` — composable building blocks. ≤ 30 LOC public surface each.
3. **Named recipes** — 10 thin vertical wrappers (5 conversational + 5 documentary) each ≤ 30 LOC, each forwarding to at most 2 primitive calls. Plus `CodingSessionMemory` — the v0 coding-agent tool-surface recipe, now a delegate over `WorkingMemory`.

All three layers sit on the same `StoragePort`, so a future backend slots in at every layer without touching the recipe surface. The public recipe surface is codegen'd to PyO3 + napi-rs from a single annotated-Rust source (`lunaris-codegen` — see Section 1), so every Rust method below has a byte-stable `snake_case` Python counterpart and `camelCase` TypeScript counterpart.

```text
                    Lunaris::recall / ingest / forget
                                  ▲
           ┌──────────────────────┼──────────────────────┐
           │                      │                      │
   MessageStream            DocumentCorpus        TemporalQuery<S>      WorkingMemory
   (recency-weighted        (RRF-fused RAG)       (typestate time-      (scratchpad +
    messages)                                      travel DSL)           consolidate)
           ▲                      ▲                      ▲                      ▲
  conversational/*         documentary/*          documentary/*         recipes::CodingSessionMemory
  (5 wrappers)             (5 wrappers)           (2 of them)
```

### 9.1 `CodingSessionMemory` — the Helios tool-surface recipe

The v0 Helios harness consumes Lunaris through this one recipe. In v0.1.1 the file was rewritten to hold a `WorkingMemory` internally and delegate every method — the public API is byte-stable versus v0.1.0 (the `coding_session_memory_public_surface_under_50_loc` test gates every commit).

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use std::sync::Arc;

use lunaris::{CodingSessionMemory, Hlc};
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let lunaris = Arc::new(lunaris::Lunaris::open("moon://localhost:6380").await?);
let pad     = CodingSessionMemory::new(lunaris.clone(), scope.clone(), "session-42");

// write(path, content) — ingest with source = "helios:fs/session-42/<path>".
pad.write("README.md", "# Hello\nWorld").await?;

// read(path) — hybrid recall over the session prefix, concatenates chunks.
let body: Option<String> = pad.read("README.md").await?;

// edit — plain write of the new content. Prior version's bt.sys[1] is stamped
// by the existing MVCC supersede path; old chunks stay visible via as_of.
pad.edit("README.md", /* old */ "", "# Hello\nUpdated").await?;

// grep(pattern, k) — hybrid recall, k hits.
let hits = pad.grep("hello", 5).await?;

// ls(prefix) — walk episode: keys, filter by session prefix, return tails.
let files = pad.ls(Some("docs/")).await?;

// forget() — BySource prefix-match soft delete.
let receipt = pad.forget().await?;

// as_of(ts) — borrowed time-travel view; re-runs read at the pinned HLC.
let ts = Hlc { wall_ms: 1_700_000_000_000, counter: 0, node_id: 0 };
let old_body = pad.as_of(ts).read("README.md").await?;
# Ok(()) }
```

Eight methods on `CodingSessionMemory` + one on `AsOfScratchpad`. Source: `crates/lunaris/src/recipes/coding_session_memory.rs` (≤ 50 LOC cap enforced by test).

**v0.1.1 addition — scoped consolidation.** In v0.1.1 the Consolidator pipeline can be turned on for the `"helios:fs/"` scope alone via `lunaris.consolidator_pipeline().enable_for_scope("helios:fs/")`. "Important notes" in the scratchpad get ACT-R-promoted to `Fact` primitives (helios-rfc §5.3) without enabling Consolidator across all tenants. Prefix match is exact — no regex, no glob. The 50/50 isolation test (`helios:fs/…` vs `other:…`) asserts zero `ConsolidatorPromotion` `AuditEvent`s for non-matching sources.

> **v0.5 note:** `HeliosScratchpad` is still available as a `#[deprecated]` type alias for `CodingSessionMemory`. It compiles with a warning and will be removed in v0.7. Migration: replace `lunaris::HeliosScratchpad` with `lunaris::CodingSessionMemory`.

### 9.2 Primitives

All four live in `lunaris-recipes` (re-exports `WorkingMemory` from `lunaris::primitives::working_memory` to avoid a dep cycle with `CodingSessionMemory`).

#### `MessageStream` — recency-weighted message recall

Recency-weighted message recall with ACT-R base-level activation (Anderson 1996, `d = 0.5`). Source: `crates/lunaris-recipes/src/message_stream.rs`.

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris_recipes::MessageStream;
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let chat = MessageStream::new(lunaris.clone(), scope.clone(), "chat:user_42/").with_top_k(10);
chat.ingest("hello world", "thread-1", "user_42").await?;
let hits = chat.recall("hello").await?;
# Ok(()) }
```

Public surface: `new`, `with_top_k`, `ingest(body, thread_id, participant_id)`, `recall(query)`. Scope prefix filters via `Filter::StartsWith` on `source`.

#### `DocumentCorpus` — RRF-fused Vector + Keyword RAG

RRF-fused Vector + Keyword RAG over a `source_prefix`-scoped document set. Source: `crates/lunaris-recipes/src/document_corpus.rs`.

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris_recipes::DocumentCorpus;
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let kb = DocumentCorpus::new(lunaris.clone(), scope.clone(), "kb:docs/");
kb.ingest(vec![
    ("# Spec\nthe quick brown fox".into(), serde_json::Map::new()),
]).await?;

// Builder pattern — filter + top cap chain, then search.
let hits = kb
    .filter("section", serde_json::Value::String("intro".into()))
    .top(5)
    .search("brown fox")
    .await?;
# Ok(()) }
```

Public surface: `new`, `ingest(chunks)`, `filter(field, value)`, `top(k)`, `search(query)`. RRF-fuses Vector + Keyword on the `chunks` index; takes the Moon-native one-trip path when the handle is opened against `moon://…`.

#### `TemporalQuery<S>` — typestate time-travel combinator

Typestate-parameterised time-travel combinator. `S` is `Messages | Documents | Facts` — different type states unlock different methods. Source: `crates/lunaris-recipes/src/temporal_query.rs`.

> **Backend note.** A *past* `as_of` hydrates through `StoragePort::read_as_of`, which needs a backend declaring `supports_historical_kv_reads() == true`. Moon does not, and is the only backend as of 0.7.0, so the call returns `StorageError::NotSupported` (HTTP `501 not_supported`): Moon stores Lunaris rows as plain hashes, so it refuses a historical pin rather than answering with present-time data. `before` / `after` / `between` filters over `valid`/`sys` on the *search* lane remain temporal (`FT.SEARCH AS_OF`).

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris_core::hlc::Hlc;
use lunaris_recipes::{Documents, TemporalQuery};
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

// as_of — point-in-time recall.
let t0 = Hlc { wall_ms: 1_700_000_000_000, counter: 0, node_id: 0 };
let hits = TemporalQuery::<Documents>::new(lunaris.clone(), scope.clone())
    .as_of(t0)
    .execute("schema v1")
    .await?;

// between(lo, hi) — closed-open range recall. hi is EXCLUSIVE.
let lo = Hlc { wall_ms: 1_700_000_000_000, counter: 0, node_id: 0 };
let hi = Hlc { wall_ms: 1_700_086_400_000, counter: 0, node_id: 0 };
let events = TemporalQuery::<Documents>::new(lunaris.clone(), scope.clone())
    .between(lo, hi)
    .execute("what happened")
    .await?;

// before / after — open-ended ranges via Filter::ValidTimeRange { after, before }.
let old = TemporalQuery::<Documents>::new(lunaris.clone(), scope.clone())
    .before(t0)
    .execute("legacy config")
    .await?;
# Ok(()) }
```

Public surface: `new`, `as_of(ts)`, `before(ts)`, `after(ts)`, `between(lo, hi)`, `execute(query)`. `between` is lower-bound inclusive, upper-bound EXCLUSIVE — for "days X..=Y inclusive" pass `hi = Y + 1_day`.

#### `WorkingMemory` — scope-prefixed scratchpad + consolidate hook

Scope-prefixed scratchpad with an explicit promotion hook. Source: `crates/lunaris/src/primitives/working_memory.rs`.

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris::WorkingMemory;  // re-exported from lunaris-recipes too
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let wm = WorkingMemory::new(lunaris.clone(), scope.clone(), "chat:user_42/");
wm.write("last_topic", serde_json::json!({"t": "memory"})).await?;
let v = wm.read("last_topic").await?;
let matches = wm.grep("memory").await?;

// Promote scratchpad events to Facts for THIS scope only.
// The Consolidator's default `consolidate_scoped` filters ConsolidateEvents
// whose source does not start with "chat:user_42/".
let report = wm.consolidate().await?;
# Ok(()) }
```

Public surface: `new`, `write(k, v)`, `read(k)`, `grep(pattern)`, `consolidate()`. The five-method cap is asserted at compile time.

### 9.3 Conversational wrappers (5)

All under `lunaris_recipes::conversational`. Each is a thin composition — ≤ 30 LOC, ≤ 2 primitive calls per public method.

#### `ChatAgentMemory` — per-user chat history

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris_recipes::conversational::ChatAgentMemory;
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let chat = ChatAgentMemory::new(lunaris.clone(), scope.clone(), "user_42");  // scope "chat:user_42/"
chat.remember("what's my name?").await?;
let hits = chat.recall("my name").await?;
# Ok(()) }
```

Public surface: `new(lunaris, scope, user_id)`, `remember(turn)`, `recall(query)`. Holds both a `MessageStream` and a `WorkingMemory` scoped at `"chat:<user_id>/"` (the same prefix so `MultiTurnConversation` can consolidate without cross-user leaks).

#### `MultiTurnConversation` — `ChatAgentMemory` + consolidation

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris_recipes::conversational::MultiTurnConversation;
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let convo = MultiTurnConversation::new(lunaris.clone(), scope.clone(), "user_42");
convo.remember("user", "hi").await?;    // (participant, body)
convo.remember("bot", "hello").await?;
let hits = convo.recall("greeting").await?;
let report = convo.consolidate().await?;  // promote notes → Facts for this user only
# Ok(()) }
```

Public surface: `new`, `remember(participant, body)`, `recall(query)`, `consolidate()`. Scope isolation enforced at BOTH the write path (`MessageStream::ingest`) and the promotion filter (`WorkingMemory::consolidate`).

#### `SlackArchive` — channel + user-filtered archive

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris_recipes::conversational::SlackArchive;
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let slack = SlackArchive::new(lunaris.clone(), scope.clone());
slack.ingest_channel("C-general", "alice", "shipping today").await?;

// Narrow to a channel → returns a fresh SlackArchive bound at "slack:archive/channel=C-general/".
let hits = slack.channel("C-general").recall("shipping").await?;

// Or use the query builder for combined filters.
let hits = slack.channel("C-general").with_user("alice").recall("shipping").await?;
# Ok(()) }
```

Public surface: `new`, `ingest_channel(channel, user, body)`, `recall(query)`, `channel(id)`, `user(id)`. `.channel(id)` returns a narrowed `SlackArchive` (scope `"slack:archive/channel=<id>/"`); `.user(id)` returns a `SlackArchiveQuery` helper with `.with_user(id)` + `.recall(query)`.

> **Parity caveat.** The current `chunks` payload shape doesn't carry `channel` / `participant_id` as top-level fields, so backend `Filter::Eq` on those fields is structurally correct but returns empty sets until the payload schema is extended. Use `.channel(id)` for reliable narrowing today — it encodes the channel into the `source` prefix, which IS fully wired end-to-end. See the module rustdoc for the full deviation note.

#### `EmailThreading` — thread-scoped email archive

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris_recipes::conversational::EmailThreading;
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let mail = EmailThreading::new(lunaris.clone(), scope.clone()).with_graph_pipeline(true);
mail.ingest("thread-1", "alice@x", "subject body").await?;
let hits = mail.thread("thread-1").recall("subject").await?;
# Ok(()) }
```

Public surface: `new`, `ingest(root_id, from, body)`, `thread(root_id)`, `recall(query)`, `with_graph_pipeline(bool)`. `.thread(root_id)` returns a fresh `EmailThreading` at scope `"email:thread/<root_id>/"`. `.with_graph_pipeline(true)` calls `lunaris.graph_pipeline().enable()` (idempotent) so Entities/Relations extracted from email bodies light up graph traversal.

#### `MeetingNotesMemory` — heading-scoped notes + attendees filter

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris_recipes::conversational::MeetingNotesMemory;
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let mtg = MeetingNotesMemory::new(lunaris.clone(), scope.clone()).with_graph_pipeline(true);
mtg.note("Q2 planning", "discussed roadmap and staffing").await?;
let hits = mtg.recall("staffing").await?;

// AND-filtered recall — every attendee must be present.
let hits = mtg
    .attendees(vec!["alice".into(), "bob".into()])
    .recall("staffing")
    .await?;
# Ok(()) }
```

Public surface: `new`, `note(heading, body)`, `recall(query)`, `attendees(list)`, `with_graph_pipeline(bool)`. `.attendees(list)` returns a `MeetingNotesQuery` that ANDs per-attendee `Filter::Eq` on `participant_id`. Default participant is `"scribe"` — for per-attendee authorship use `MessageStream::ingest` directly.

### 9.4 Documentary wrappers (5)

All under `lunaris_recipes::documentary`. Each composes `DocumentCorpus` and/or `TemporalQuery` and/or `MessageStream`.

#### `DocumentKnowledgeBase` — filtered RAG over a source prefix

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris_recipes::documentary::DocumentKnowledgeBase;
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let kb = DocumentKnowledgeBase::new(lunaris.clone(), scope.clone(), "kb:docs/");
kb.ingest(vec![
    ("# Onboarding\nStart here.".into(), serde_json::Map::new()),
]).await?;
let hits = kb
    .filter("section", serde_json::Value::String("intro".into()))
    .top(5)
    .search("onboarding")
    .await?;
# Ok(()) }
```

Public surface: `new`, `ingest(chunks)`, `filter(field, value)`, `top(k)`, `search(query)`. One-to-one passthrough of `DocumentCorpus`; no business logic.

#### `ResearchPaperCorpus` — `DocumentCorpus` + opt-in citation graph

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
# let chunks: Vec<(String, serde_json::Map<String, serde_json::Value>)> =
#     vec![("a paper chunk".into(), serde_json::Map::new())];
use lunaris_recipes::documentary::ResearchPaperCorpus;
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let papers = ResearchPaperCorpus::new(lunaris.clone(), scope.clone(), "papers:")
    .with_graph_pipeline(true);        // opt-in citation graph
papers.ingest(chunks).await?;
let hits = papers.search("attention is all you need").await?;
# Ok(()) }
```

Public surface: `new`, `with_graph_pipeline(bool)`, `ingest(chunks)`, `search(query)`. Graph opt-in is per-handle and idempotent; default OFF per blueprint §5.2.

#### `CodeRepoMemory` — function body "as-of commit N"

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris_core::hlc::Hlc;
use lunaris_recipes::documentary::CodeRepoMemory;
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let repo = CodeRepoMemory::new(lunaris.clone(), scope.clone(), "repo:lunaris/");
// Ingest one commit; wrapper stamps commit_sha + committer_date_unix_ms.
repo.ingest_commit(
    "ae7b60e",
    1_713_700_000_000,
    vec![("fn recall() { ... }".into(), serde_json::Map::new())],
).await?;

// Time-travel to a specific commit — Hlc carried directly for counter/node_id control.
let ts = Hlc { wall_ms: 1_713_700_000_000, counter: 0, node_id: 0 };
let hits = repo.recall("recall", ts).await?;
# Ok(()) }
```

Public surface: `new`, `ingest_commit(commit_sha, committer_date_unix_ms, chunks)`, `recall(query, as_of)`. `committer_date_unix_ms` is `i64`; precision below nanos has nowhere to live on `Hlc`. Isolation across repos is the caller's responsibility — `TemporalQuery` recalls across all Documents, so use a unique `repo_prefix` per repo.

#### `TimelineReconstruction` — "what happened on day X"

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
# let chunks: Vec<(String, serde_json::Map<String, serde_json::Value>)> =
#     vec![("an event".into(), serde_json::Map::new())];
# let day_0_ms: u64 = 1_736_467_200_000;
use lunaris_core::hlc::Hlc;
use lunaris_recipes::documentary::TimelineReconstruction;
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let timeline = TimelineReconstruction::new(lunaris.clone(), scope.clone(), "timeline:events/");
timeline.ingest(chunks).await?;

let start = Hlc { wall_ms: day_0_ms, counter: 0, node_id: 0 };
let end   = Hlc { wall_ms: day_0_ms + 86_400_000, counter: 0, node_id: 0 };
// between(lo, hi) — EXCLUSIVE upper bound: pass hi = last_day + 1_day for inclusive.
let day_hits = timeline.between("incident", start, end).await?;
let at_hits  = timeline.as_of("incident", start).await?;
# Ok(()) }
```

Public surface: `new`, `ingest(chunks)`, `between(query, lo, hi)`, `as_of(query, ts)`. Deliberately thin (< 10 LOC acceptable) — value is named discoverability.

#### `CustomerSupportHistory` — tickets + chats, RRF within each

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
use lunaris_recipes::documentary::CustomerSupportHistory;
use lunaris::Scope;

let scope = Scope::new("acme-prod")?;   // RFC 0001 partition key

let hist = CustomerSupportHistory::new(lunaris.clone(), scope.clone())
    .with_graph_pipeline(true);       // opt-in product-customer relations
hist.ingest_ticket("T-101", "login fails after the 2.3 upgrade").await?;
hist.ingest_chat("T-101", 0, "customer", "app crashes on start").await?;
let hits = hist.recall("crash").await?;     // concat: tickets first, chats second
# Ok(()) }
```

Public surface: `new`, `with_graph_pipeline(bool)`, `ingest_ticket(id, body)`, `ingest_chat(ticket_id, turn_idx, from, body)`, `recall(query)`. Critical contract: RRF fuses **within** each primitive's bucket, not across types. The wrapper asserts the two source prefixes are distinct (`ticket:` vs `chat:`) so double-indexed records can't collapse duplicates.

### Gotchas

- **One wrapper ≠ one primitive call.** Each wrapper method forwards to at most 2 primitive calls. Anything more is business logic that belongs in application code, not in the wrapper (D-04 / RCPDOC-05 contract).
- **Scope prefixes are load-bearing.** `ChatAgentMemory` + `MultiTurnConversation` share `"chat:<user_id>/"` across `MessageStream` AND `WorkingMemory` precisely so `consolidate()` can filter on source prefix and reject cross-user events. Never re-scope one of the two primitives without the other.
- **Graph-on is still default-off.** Every `.with_graph_pipeline(true)` call hits the same `GraphPipelineHandle` — you're toggling the handle, not the wrapper. Calling it on one wrapper flips graph on for every ingest in the process. Idempotent; matches blueprint §5.2.
- **`between` is lower-inclusive, upper-EXCLUSIVE.** Applies to `TemporalQuery::between` and `TimelineReconstruction::between` both. For "days X..=Y inclusive" pass `hi = Y + 1_day`.
- **HeliosScratchpad is still filesystem-shaped.** `grep` always uses hybrid + rerank; `read` always concatenates chunks; `edit` ignores its `_old` argument (MVCC retention keeps prior versions visible via `as_of`); `ls` is an O(all-episodes) prefix walk over `episode:` keys — fine session-scoped, not fine tenant-wide.
- **`CustomerSupportHistory.recall` returns a concatenation, not a fused list.** Tickets first, chats second. Tie-bucket ordering is accepted as known-flakiness (top-k set equality is the gate, per D-13).
- **Cross-language API is codegen'd.** Do not hand-write `lunaris-py` or `lunaris-ts` wrapper classes. Edit Rust, run `cargo run -p lunaris-codegen -- --dump-ir`, commit the IR snapshot — CI fails the PR when Rust/Py/TS drift. The Python surface is `snake_case`; the TypeScript surface is `camelCase`; both are stable versus the Rust surface at every wrapper method name and argument order.

---

## 10. Troubleshooting

### `llamacpp embedder failed to open; falling through to the remote/Noop chain` (vector recall returns empty)

This is a `WARN`, not a hard error — `open` succeeds with a zero-vector
`NoopEmbedder` and recall returns empty rows until the GGUF is staged.
Download the embedder GGUF to
`~/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf`
(or point `LUNARIS_EMBEDDER_GGUF` at an existing copy) and verify it
against the canonical SHA-256 printed by
`cargo run -p lunaris-bench --bin stage-models -- --help`. The MCP server
stages it automatically on first recall.

Or (air-gap escape hatch, requires `--features embed-remote`):

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
# let url = "moon://localhost:6380";
let lunaris = lunaris::Lunaris::open(url).await?
    .with_embedder(Arc::new(lunaris_embed_remote::OllamaEmbedder::new(Default::default())?));
# Ok(()) }
```

### Reranker GGUF missing → `NoopReranker`

Same pattern — stage `~/.lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf`
(override: `LUNARIS_RERANKER_GGUF`) or call `handle.with_reranker(...)` with a custom impl. The reranker loads lazily on first recall; the log is informational and recall keeps working without the cross-encoder pass.

### `LunarisError::Storage(StorageError::UnsupportedScheme("..."))`

Your URL scheme isn't `moon` (`crates/lunaris/src/open.rs`). Check for typos (`redis://`, `mon://`), and note that the pre-0.7 spellings are gone: the error carries the migration link. Lunaris does not auto-detect — it matches on the scheme string.

### Empty recall results

The most common cause is an over-tight `filter_str`. Drop the filter:

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
let hits = lunaris.recall()
    .with_root(Vector::new("chunks", 30).top(5))
    // .filter_str("source LIKE 'helios:fs/wrong/%'").unwrap()  // remove this
    .execute(Query::text("..."))
    .await?;
# Ok(()) }
```

The second-most-common cause is asking for an index that wasn't written — the chunker fills `chunks`, the extractor fills `entities` and `facts`, and RAPTOR fills `communities` with embedded summary nodes at ingest — queryable via the `Tree` operator (the consolidator's Leiden run also contributes community nodes).

### Every hit has `degraded=true`

Verifier queue is backed up beyond `LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` (default 1000). Check:

```bash
curl -s http://localhost:8080/metrics | grep verify_queue_depth
tracing::debug!(verify_queue_depth = ...)   # set RUST_LOG=debug
```

Remediation is a capacity call — either scale the verifier backend, raise the threshold, or disable the verifier pipeline until the backlog clears.

### `cannot find type Vector / Graph / Keyword in scope`

You're reaching into the inner crate path. Use the umbrella re-exports:

```rust
use lunaris::{EntityId, Graph, Keyword, NoopExtractor, Query, Vector};
```

Every symbol the guide names is re-exported at the `lunaris::` top level from `crates/lunaris/src/lib.rs:34-107`.

### `hard-delete requires confirmation token from prior dry_run`

You called `.hard()` without attaching a `ForgetConfirmation`. Two-step flow:

```rust,no_run
# use std::sync::Arc;
# use lunaris::{EntityId, Graph, Keyword, Lunaris, Query, Scope, Vector};
# async fn demo() -> Result<(), Box<dyn std::error::Error>> {
# let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
# let scope = Scope::new("acme-workspace")?;
# let _ = &scope;
# use lunaris::{ForgetTarget, ScopeSpec};
# let target = ForgetTarget::Scope(ScopeSpec::BySource("helios:fs/session-42/".into()));
let preview = lunaris.forget(target.clone().dry_run()).await?;
let token   = lunaris.confirm_hard_forget(preview).await?;
let _       = lunaris.forget(target.hard().with_token(token)).await?;
# Ok(()) }
```

### I want a test without downloading any model

`Lunaris::with_parts(storage, Arc::new(StubEmbedder::new(768)), clock)` — this is how `crates/lunaris/tests/ingest_smoke.rs:91-108` runs. `StubEmbedder` emits deterministic 768-dim vectors with no forward pass. For keyword-aware tests, use `with_parts_keyword` and supply a `RecordingStorageWithKeyword`-style fixture (`crates/lunaris/tests/recall_smoke.rs:180-217`).

---

## Next steps

- **Wire protocol & conformance:** [`docs/protocol/memoryprotocol-0.1.md`](protocol/memoryprotocol-0.1.md) is the source of truth for HTTP JSON shapes, the SSE contract, the error taxonomy, and how to certify a third-party server.
- **Runnable examples:** the shortest path to a working `.rs` file is the integration tests at `crates/lunaris/tests/`:
  - `ingest_smoke.rs` — the `with_parts` + `StubEmbedder` ingest pattern (Section 2 anchor).
  - `recall_smoke.rs` — the BM25 + vector recall pattern (Section 3 anchor).
  - `graph_pipeline_smoke.rs` — the `LUNARIS_GRAPH_ENABLED` round-trip (Section 4 anchor).
  - `coding_session_memory_smoke.rs` — dual-backend recipe smoke test (Section 9 anchor).
- **Operational envelope:** `.planning/architect/blueprint.md` for the full latency/throughput targets; `docs/protocol/conformance.md` for the cross-backend conformance contract.
- **Benchmarks:** `cargo xtask bench` drives the Plan 02-04 + 03-04 budget harness. Live numbers land per the 03-HUMAN-UAT and 05-HUMAN-UAT runbooks in `.planning/phases/`.
