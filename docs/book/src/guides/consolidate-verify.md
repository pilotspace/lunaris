# Consolidation & Verification (opt-in)

**Reach for this chapter when your deployment has latency budget to spare and
you want quality/provenance signals landing in the audit stream.** These are
the two opt-in *slow paths* — neither runs on the recall hot path. Both ship
**default OFF** (blueprint §5.1); turn them on once your queue-lag SLOs hold.

| Pipeline | Handle | Backend env | Enable env | What it does |
|---|---|---|---|---|
| Consolidate | `ConsolidatorPipelineHandle` | `LUNARIS_CONSOLIDATOR_BACKEND` (`actr`/`noop`) | `LUNARIS_CONSOLIDATE_ENABLED` | ACT-R activation, promotion/archival, Leiden communities |
| Verify | `VerifierPipelineHandle` | `LUNARIS_VERIFIER_BACKEND` (`270m`/`small` \| `27b`/`large` \| `noop`) | `LUNARIS_VERIFY_ENABLED` | Slow-path arbitration of contradicting / invalid primitives |

Backend *availability* is gated by Cargo features; the env vars only choose
among what was compiled in. See
[Configuration Reference](../reference/configuration.md).

## The consolidator (ACT-R)

`lunaris-consolidate` consumes the `__lunaris_consolidate__` queue (one
message per ingest commit, consumer group `lunaris-consolidate-v0`), debounces
per `episode_id`, and runs a consolidation pass:

- **ACT-R base-level activation** — Anderson 1996, decay `d = 0.5`, with the
  Petrov 2006 O(1) incremental approximation (`ActRScorer`,
  `crates/lunaris-consolidate/src/act_r.rs`). High-activation
  episodes/notes get **promoted** to `Fact` primitives; stale facts get
  **archived**.
- **Leiden community detection** — a hand-rolled label-propagation pass
  (`leiden_pass`, `crates/lunaris-consolidate/src/leiden.rs`) over the graph,
  producing `Community` primitives (the `communities` recall index). Rustworkx
  is deliberately rejected — it carries `unsafe` blocks.

The crate has **no LLM backends** — community summaries are produced by the
Phase-3 `Extractor` acting as a summarizer, wired at the umbrella handle.

```rust
lunaris.consolidator_pipeline().enable();
```

`ConsolidatorPipelineHandle` (`crates/lunaris/src/consolidator_pipeline.rs`)
exposes `enable()` / `disable()` / `is_enabled()`, `set_consolidator(arc)`,
`bind_storage(arc)`, `join_worker()`, `state_change_count()`, **and** —
uniquely among the three pipeline handles — `enable_for_scope(prefix)`:

```rust
// Promote only events whose source starts with "helios:fs/" — Consolidator
// stays off for every other tenant. Prefix match is exact: no regex, no glob.
lunaris.consolidator_pipeline().enable_for_scope("helios:fs/");
```

The prefix is a **source-prefix filter on the consolidate-event stream**, not
a `Scope` partition key — `Consolidator::consolidate_scoped` drops events
whose `event.source` doesn't start with it before forwarding to
`consolidate()` (`crates/lunaris-consolidate/src/lib.rs`). An empty prefix is
rejected. `lunaris_recipes::WorkingMemory::consolidate()` and the
`HeliosScratchpad` recipe use this path (see
[Cookbook → Helios Scratchpad](../cookbook/helios-scratchpad.md)).

Per-event audit records match the `AuditEvent` enum verbatim — one
`ConsolidatorPromotion { episode_id, fact_id, activation_score }` per
promotion, one `ConsolidatorArchive { fact_id, final_activation, moved_to }`
per archive. There is no rolled-up "report" audit variant.

## The verifier (slow-path arbitration)

`lunaris-verify` consumes the `__lunaris_verify__` queue (emitted by the
[graph-on ingest path](./graph.md), consumer group `lunaris-verify-v0`). For
each `NeedsReviewItem` it produces a `VerifyDecision` naming the winner /
loser / reason; a non-deferred decision flows through **one** `atomic_write`
(the MVCC supersede invariant — the loser's `bt.sys_to` is stamped via
`read_as_of` + `BiTemporal::invalidate_sys`), followed by one fire-and-forget
audit publish.

```rust
lunaris.verify_pipeline().enable();
```

`VerifierPipelineHandle` (`crates/lunaris/src/verify_pipeline.rs`) exposes
`enable()` / `disable()` / `is_enabled()`, `set_verifier(arc)`,
`bind_storage(arc)`, `bind_clock(arc)`, `join_worker()`. No `enable_for_scope`
— the verifier runs handle-wide. Returning `VerifyDecision::deferred()` is
"abstain": the worker skips the supersede write for that item.

### RFC 0006 — backend selector vs the effective verifier

`LUNARIS_VERIFIER_BACKEND` *defaults to `270m`* (the RFC 0006 laptop floor),
but the **effective verifier** is `NoopVerifier` unless two things are true:
the matching Cargo feature is built **and** the model weights are staged in
`~/.cache/lunaris/models/`. With stock features (`lunaris-verify` ships
`default = []`; the `lunaris` umbrella's default `candle` feature forwards the
27B backend, not `verify-small`) the `270m` selector resolves to "try 27B" →
cache miss on a dev box → a `tracing::warn!` → `NoopVerifier`. So out of the
box, *and any time the resolved backend's weights are missing*, the verifier
worker runs the no-op (no crash, no useful output) — a "deferred" decision the
worker treats as "skip". To get a *real* verifier on a laptop, build with
`--features verify-small` and stage the 270M weights.

| Cargo feature | Backend | `LUNARIS_VERIFIER_BACKEND` value |
|---|---|---|
| *(none — `lunaris-verify` crate's own default `[]`)* | `NoopVerifier` only | `noop` |
| `verify-small` (pulls in `candle`) | `CandleGemma3_270M` — **the laptop-floor build** (~600 MB disk / ~1 GB RAM, no Ollama) | `270m` / `small` |
| `candle` (or `verify-large`, its alias — this is what the `lunaris` umbrella's default `candle` forwards) | `CandleGemma3_27B` — the slow-path "get it right" model (7–10× slower than the 4B extractor); ~14 GiB weights | `27b` / `large` |
| `ollama` | `OllamaVerifier` (HTTP `/api/chat`) | — |
| `cloud-api` | `CloudApiVerifier` (Anthropic `claude-3-5-sonnet-latest` / OpenAI `gpt-4o-2024-11-20` / Gemini `gemini-1.5-pro-latest`) via `LUNARIS_VERIFY_PROVIDER` + `LUNARIS_VERIFY_API_KEY` | — |

The **verify-small** build is the recommended way to run a *real* verifier on
a laptop without Ollama: `cargo build --features verify-small`, then
`LUNARIS_VERIFIER_BACKEND=270m LUNARIS_VERIFY_ENABLED=1`.

## Surfacing backlog to readers — `recall_with_degraded_check`

The verifier is asynchronous, so recall results can be stale relative to
pending arbitrations. `Lunaris::recall_with_degraded_check()` reads the
verifier queue depth **once** and, if it exceeds
`LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` (default 1000,
`crates/lunaris/src/recall.rs:26-31`), seeds the builder so every returned
`Hit::degraded` is `true`:

```rust
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
```

It is best-effort: if the backend's `queue_depth` returns `NotSupported`, the
call falls through with `degraded = false` and still returns hits.

## Gotchas

- **Both pipelines default OFF.** `.enable()` with no real backend installed
  runs `NoopConsolidator` / `NoopVerifier` — no crashes, no work done. Wire a
  backend (`with_consolidator` / `with_verifier`, or the right Cargo feature +
  env) first.
- **Apply component swaps before `.enable()`** — `.enable()` snapshots the
  current component; later swaps propagate via `set_*`.
- **Queue topics are hard-coded constants** (`__lunaris_consolidate__`,
  `__lunaris_verify__`, `crates/lunaris/src/ingest.rs:48-54`). The shipped
  workers use the `-v0` consumer groups so a future schema bump can land on a
  fresh group.
- **Per-scope supervisors exist** but the pipeline *handles* still drive the
  deprecated single-topic workers in v0.2.x; the supervisor migration is a
  v0.3 item (RFC 0001 §11.6). See [Multi-Agent & Scope](./multi-agent.md) for
  `LUNARIS_SCOPE_CONCURRENCY` etc.

## See also

- [The Graph Pipeline](./graph.md) — produces the `__lunaris_verify__`
  messages.
- [Ingesting Observations](./ingest.md) — produces the `__lunaris_consolidate__`
  messages.
- [Configuration Reference](../reference/configuration.md) — every feature
  flag and `LUNARIS_*` env var.
