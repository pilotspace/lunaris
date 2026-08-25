# Consolidation & Verification (opt-in)

**Reach for this chapter when your deployment has latency budget to spare and
you want quality/provenance signals landing in the audit stream.** These are
the two opt-in *slow paths* — neither runs on the recall hot path. Both ship
**default OFF** (blueprint §5.1); turn them on once your queue-lag SLOs hold.

| Pipeline | Handle | Backend env | Enable env | What it does |
|---|---|---|---|---|
| Consolidate | `ConsolidatorPipelineHandle` | `LUNARIS_CONSOLIDATOR_BACKEND` (`actr`/`noop`) | `LUNARIS_CONSOLIDATE_ENABLED` | ACT-R activation, promotion/archival, Leiden communities |
| Verify | `VerifierPipelineHandle` | `LUNARIS_VERIFY_PROVIDER` (`anthropic`\|`openai`\|`gemini`\|`minimax`\|`openai-compat`, else `Noop`) | `LUNARIS_VERIFY_ENABLED` | Slow-path arbitration of contradicting / invalid primitives |

The verifier is **remote-only** since the v0.6 llama.cpp-only cutover — see
`docs/decisions/2026-07-10-llamacpp-only-cutover.md` (the cutover ADR). No
local model tiers to build or stage; a set-but-broken provider degrades
loudly to `NoopVerifier` (warn), never a silent backend swap. See
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

```rust,no_run
# use lunaris::Lunaris;
# async fn demo() -> Result<(), lunaris::LunarisError> {
# let lunaris = Lunaris::open("moon://localhost:6380").await?;
lunaris.consolidator_pipeline().enable();
# Ok(())
# }
```

`ConsolidatorPipelineHandle` (`crates/lunaris/src/consolidator_pipeline.rs`)
exposes `enable()` / `disable()` / `is_enabled()`, `set_consolidator(arc)`,
`bind_storage(arc)`, `join_worker()`, `state_change_count()`, **and** —
uniquely among the three pipeline handles — `enable_for_scope(prefix)`:

```rust,no_run
# use lunaris::Lunaris;
# async fn demo() -> Result<(), lunaris::LunarisError> {
# let lunaris = Lunaris::open("moon://localhost:6380").await?;
// Promote only events whose source starts with "helios:fs/" — Consolidator
// stays off for every other tenant. Prefix match is exact: no regex, no glob.
lunaris.consolidator_pipeline().enable_for_scope("helios:fs/");
# Ok(())
# }
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

```rust,no_run
# use lunaris::Lunaris;
# async fn demo() -> Result<(), lunaris::LunarisError> {
# let lunaris = Lunaris::open("moon://localhost:6380").await?;
lunaris.verify_pipeline().enable();
# Ok(())
# }
```

`VerifierPipelineHandle` (`crates/lunaris/src/verify_pipeline.rs`) exposes
`enable()` / `disable()` / `is_enabled()`, `set_verifier(arc)`,
`bind_storage(arc)`, `bind_clock(arc)`, `join_worker()`. No `enable_for_scope`
— the verifier runs handle-wide. Returning `VerifyDecision::deferred()` is
"abstain": the worker skips the supersede write for that item.

### Remote-only verifier (v0.6 llama.cpp-only cutover)

RFC 0006 originally shipped a candle-based laptop floor (`verify-small`,
Gemma-3-270M) vs a "get it right" tier (`verify-large`, Gemma-3-27B). Both
were deleted in the v0.6 llama.cpp-only cutover — see
`docs/decisions/2026-07-10-llamacpp-only-cutover.md` (the cutover ADR). There
is no in-process verifier model anymore; `LUNARIS_VERIFY_PROVIDER` selects a
remote provider, or the effective verifier is `NoopVerifier`:

| `LUNARIS_VERIFY_PROVIDER` value | Backend | Notes |
|---|---|---|
| *(unset)* | `NoopVerifier` | No crash, no work done — the safe default |
| `anthropic` \| `openai` \| `gemini` \| `minimax` | Cloud-API verifier via the matching provider SDK | Needs the provider's API key env var |
| `openai-compat` | Generic OpenAI-compatible HTTP verifier | `LUNARIS_OPENAI_COMPAT_BASE_URL` (keyless allowed); covers Ollama, llama-server, vLLM, LM Studio |

A provider that is set but fails to construct (bad URL, missing key) logs a
`tracing::warn!` and degrades to `NoopVerifier` — it never silently falls
back to a different backend. Rust callers can still supply any custom impl
via `with_verifier`.

## Surfacing backlog to readers — `recall_with_degraded_check`

The verifier is asynchronous, so recall results can be stale relative to
pending arbitrations. `Lunaris::recall_with_degraded_check()` reads the
verifier queue depth **once** and, if it exceeds
`LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` (default 1000,
`crates/lunaris/src/recall.rs:26-31`), seeds the builder so every returned
`Hit::degraded` is `true`:

```rust,no_run
# use lunaris::{Lunaris, Query, Vector};
# async fn demo() -> Result<(), lunaris::LunarisError> {
# let lunaris = Lunaris::open("moon://localhost:6380").await?;
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
# Ok(())
# }
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
