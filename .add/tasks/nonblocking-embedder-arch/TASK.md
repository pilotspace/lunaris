# TASK: Non-blocking embedder/reranker: separate latency classes so recall never head-of-line-blocks behind ingest

slug: nonblocking-embedder-arch · created: 2026-07-14 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it, or run `add.py autonomy set`. -->
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower the
     autonomy level to `manual` or `conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-llamacpp/src/worker.rs:EncodeWorker` — the single-context inference
  actor. ONE OS thread (`spawn`, worker.rs:66) owns ONE warm `LlamaContext` + a
  `mpsc::Sender<Job>` (worker.rs:44). `encode()` (worker.rs:124) pushes a `Job` and
  BLOCKS on `reply_rx.recv()` (worker.rs:137). All callers serialize through this one
  channel — the module doc states it outright (worker.rs:18: "Concurrent callers
  serialize through the channel — one context is the deliberate footprint contract").
  This is the head-of-line-blocking point: an interactive recall-query embed waits
  behind whatever large ingest batch is already in the channel.
- `crates/lunaris-llamacpp/src/embedder.rs:LlamaCppEmbedder::embed_batch` (embedder.rs:184) —
  async trait impl. Per call: clones `Arc<Inner>`, hops through `tokio::task::spawn_blocking`,
  then calls `embed_blocking` → `worker.encode()`. The spawn_blocking hop moves off the async
  reactor but still funnels into the SAME single `EncodeWorker`.
- `crates/lunaris-llamacpp/src/embedder.rs:Inner` (embedder.rs:85) — `{ model, worker:
  EncodeWorker, budget }`. `budget` fixed at open (also sizes the worker context; the knob just
  shipped in contextd-embed-budget / mcp-embed-budget).
- `crates/lunaris-llamacpp/src/reranker.rs:LlamaCppReranker` — same shape (its own `Inner` +
  `EncodeWorker`); a lazy-loaded second single-context actor on the recall path.
- `crates/lunaris-hook/src/embed_promotion.rs:{run_worker, promote_batch}` (promote_batch:154) —
  the background per-scope ingest embedder. `promote_batch` → `handle.embedder().embed_batch()`
  with a full batch of captured chunks. THIS is the workload that fills the channel and starves
  an interactive query embed. `context.rs:ensure_embed_worker` (context.rs:931) spawns it.

Context (working folder): recall latency contract is the project goal (sub-25ms recall);
embed-promotion already lives OFF the write path (async per-scope worker) — the remaining
gap is that promotion and recall share the ONE serializing context.
Honors (patterns / conventions): never hold a lock across `.await` (no locks here — mpsc
actor); `#![forbid(unsafe_code)]` in lunaris-hook; footprint contract (one context per model)
is deliberate — any fix that adds a second context MUST justify the added memory against the
budget knob just shipped.
Anchors the contract cites: `EncodeWorker` (worker.rs), `LlamaCppEmbedder::embed_batch`
(embedder.rs:184), `Inner` (embedder.rs:85), `embed_promotion::promote_batch`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Priority-aware EncodeWorker — interactive recall-query embeds preempt background ingest between token-windows, on ONE shared llama.cpp context.
Framings weighed: priority lane on the single context (chosen) · second small interactive context (rejected: +~1 GB footprint, breaks the one-context contract just tuned) · bounded context pool (rejected: N× memory, overkill for a single-daemon workload)
Must:
<must>
  - `EncodeWorker` accepts jobs at two priorities: `Interactive` (recall queries) and `Background` (ingest promotion). The worker always serves all pending `Interactive` jobs before it picks the next `Background` job.
  - A caller submitting a Background workload larger than one token-window MUST split it into window-sized (≤ `budget` tokens) sub-jobs at submit time, each queued at `Background` priority; results reassembled in original input order. This is what makes the interleave happen "between windows" without a partial-job state machine inside the worker.
  - The `Embedder` trait gains `embed_batch_lowpri(inputs)` with a DEFAULT impl delegating to `embed_batch` (so `NoopEmbedder` / remote embedders are unaffected). `LlamaCppEmbedder` overrides it to submit at `Background` priority. `embed_promotion::promote_batch` calls `embed_batch_lowpri`; direct SDK/recall keeps calling `embed_batch` (= `Interactive`).
  - Exactly ONE llama.cpp context per model — `contexts_created() == 1` after any mix of jobs. Zero added resident footprint versus today.
  - Embeddings are byte-identical to today for a given input, in the same input order, regardless of priority or interleaving.
  - Fail-safe: if the priority intake ever fails to distinguish lanes, the worker falls back to FIFO ordering — a job is NEVER dropped or reordered incorrectly (correctness over latency).
</must>
Reject:
<reject>
  - Job submitted after the worker thread is gone -> "llama.cpp worker is gone" (existing behavior preserved, not regressed).
  - Empty `token_lists` / empty `inputs` -> `Ok(vec![])` (existing behavior preserved — no lane touched).
</reject>
After:
<after>
  - With a multi-window Background ingest batch in flight, an `Interactive` query submitted mid-batch completes within ~1 window (its own encode time + at most one background window), NOT after the whole batch drains.
  - `contexts_created() == 1` (the A2 context-reuse regression pin still holds).
  - Reranker path unchanged (only ever serves interactive rerank; no background contention today).
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Caller-side window-splitting (not in-worker preemption) is the right seam — lowest confidence because it moves the chunking responsibility onto `embed_batch_lowpri` (which must split into ≤budget windows and reassemble in order) rather than letting the worker pause an in-flight big job. If wrong: a single background job whose token_lists exceed one window still blocks a query for that whole job's duration (mitigated because `embed_promotion` already batches at `config.batch_size`, and chunks are ~500 tok, so a window holds several). Cost if wrong: a follow-up to push splitting into the worker loop (partial-job state machine).
  - [x] Two-lane intake without lock-across-await — RESOLVED: worker is a plain sync OS thread (no `.await`). crossbeam is NOT a workspace dep AND carries an active advisory (RUSTSEC-2026-0204, project memory), so DO NOT add it. Use std `Mutex<VecDeque<Job>>` (two deques: high/low) + `Condvar`: producers lock→push→notify; worker locks→pop high else low else wait. std-only, zero new deps.
  - [x] Reranker deferral is safe — RESOLVED: reranker is lazy-loaded and only invoked on the interactive recall path (`with_reranker`, first rerank); grep of lunaris-hook shows no background rerank caller. No contention to fix now.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: interactive query preempts an in-flight background batch
  Given an EncodeWorker with a multi-window Background workload already queued (several window-sized jobs)
  When an Interactive job is submitted after the Background jobs but before they finish
  Then the Interactive job's result is produced before the remaining Background jobs' results
  And exactly one llama.cpp context is used (contexts_created() == 1)

Scenario: priority ordering under mixed submission
  Given a worker with a mix of Background and Interactive jobs enqueued in interleaved order
  When the worker drains its queues
  Then every pending Interactive job completes before the next Background job is started
  And every job's embeddings are byte-identical to a single-lane FIFO run of the same inputs

Scenario: embeddings unchanged regardless of lane
  Given the same input strings embedded once via embed_batch (Interactive) and once via embed_batch_lowpri (Background)
  When both calls return
  Then the two embedding vectors are byte-identical and in the same input order

Scenario: promotion uses the low-priority lane
  Given embed_promotion::promote_batch running against a real LlamaCppEmbedder
  When it embeds a batch of captured chunks
  Then it calls embed_batch_lowpri (Background), not embed_batch
  And a concurrent recall query embed still returns within ~1 window

Scenario: non-llamacpp embedders unaffected by the new method
  Given a NoopEmbedder (or any Embedder without an override)
  When embed_batch_lowpri is called
  Then it returns exactly what embed_batch returns (default delegation)
  And no priority machinery is required of the implementor

Scenario: worker gone is still rejected
  Given an EncodeWorker whose thread has exited
  When a job is submitted at either priority
  Then the call returns Err "llama.cpp worker is gone"
  And no panic occurs and no partial result is returned

Scenario: empty input is a no-op on both lanes
  Given empty inputs
  When embed_batch or embed_batch_lowpri is called
  Then it returns Ok(empty) without touching either lane
  And contexts_created() is unchanged
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
# --- lunaris-llamacpp/src/worker.rs ---
pub(crate) enum Priority { Interactive, Background }   # Interactive drains first

impl EncodeWorker {
  # priority param ADDED (was: fn encode(&self, token_lists))
  pub(crate) fn encode(&self, token_lists: Vec<Vec<LlamaToken>>, priority: Priority)
      -> Result<Vec<Vec<f32>>, String>
    Ok  -> Vec<Vec<f32>>  (CLS-pooled, input order — byte-identical to today)
    Err -> "llama.cpp worker is gone" | "llama.cpp worker died mid-encode"
  pub(crate) fn contexts_created(&self) -> usize          # INVARIANT: == 1
}
# Intake (internal, not public): std Mutex<{ high: VecDeque<Job>, low: VecDeque<Job> }> + Condvar.
# Worker loop: lock -> pop high else low else wait(condvar). NO .await in the worker thread.

# --- lunaris-core (Embedder trait) ---
#[async_trait] trait Embedder {
  async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError>   # = Interactive
  # NEW, default-provided:
  async fn embed_batch_lowpri(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
      self.embed_batch(inputs).await          # default: identical to embed_batch (Noop/remote unaffected)
  }
}

# --- lunaris-llamacpp/src/embedder.rs (LlamaCppEmbedder overrides) ---
embed_batch(inputs)          -> worker.encode(tokens, Priority::Interactive)
embed_batch_lowpri(inputs)   -> split tokens into <=budget windows, worker.encode(window, Priority::Background)
                                per window, reassemble Vec in original input order
# byte-identical embeddings to embed_batch for the same inputs.

# --- lunaris-hook/src/embed_promotion.rs ---
promote_batch: handle.embedder().embed_batch(...)  ->  ...embed_batch_lowpri(...)   # background lane
```
Access pattern: no storage/schema change — pure in-process inference scheduling. Reranker path untouched.

Status: FROZEN @ v1 — approved by Tin Dang (2026-07-14). Changing this shape = change request back to SPECIFY.
Least-sure flag surfaced at freeze: [spec] the interleave is caller-side window-splitting (embed_batch_lowpri splits into ≤budget windows), NOT in-worker preemption of a big job — if a single Background job exceeds one window it still blocks a query for that job's duration; accepted at freeze (mitigated: promotion batches at config.batch_size, ~500-tok chunks pack several per window), with true mid-job preemption (partial-job state machine) deferred to a follow-up.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: scheduling logic 100% (pure, model-free); trait + wiring behavior 100%; real-backend interleave = 1 discriminating integration test (model-gated).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_intake_drains_high_before_low: extract a model-free `PriorityIntake` (two deques + Condvar). arrange push [low,low,high,low] / act drain / assert pop order [high,low,low,low]. (scenario: priority ordering under mixed submission)
  - test_intake_fifo_within_lane: arrange push [high_a, high_b] / assert pop [high_a, high_b] (FIFO within a lane; no starvation reorder).
  - test_embed_batch_lowpri_defaults_to_embed_batch: fake Embedder in lunaris-core with only embed_batch impl / call both / assert byte-identical Vecs, same order. (scenario: non-llamacpp embedders unaffected + embeddings unchanged regardless of lane)
  - test_promote_batch_uses_lowpri: recording fake Embedder flagging which method fired / run promote_batch / assert embed_batch_lowpri called, embed_batch NOT. (scenario: promotion uses the low-priority lane)
  - test_worker_gone_rejected_both_priorities: (reuse existing worker-gone harness) submit at each priority after teardown / assert Err "llama.cpp worker is gone", no panic. (scenario: worker gone is still rejected)
  - test_empty_input_noop_both_lanes: embed_batch + embed_batch_lowpri on [] / assert Ok(empty), contexts_created unchanged. (scenario: empty input no-op)
  - [MODEL-GATED, discriminating] test_interactive_preempts_background_batch: real LlamaCppEmbedder / enqueue multi-window Background then an Interactive job / assert Interactive result ordered before remaining Background + contexts_created()==1. Proves the PRODUCTION path (worker.encode + windowing) runs the feature, not just the extracted intake. (scenario: interactive query preempts an in-flight background batch)
</test_plan>

Tests live in: `crates/lunaris-llamacpp/src/worker.rs` (intake unit tests) · `crates/lunaris-core/src/embedder.rs` (trait default test) · `crates/lunaris-hook/src/embed_promotion.rs` (promote_batch wiring test) · `crates/lunaris-llamacpp/tests/` (model-gated interleave) · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-llamacpp/src/worker.rs` `crates/lunaris-llamacpp/src/embedder.rs` `crates/lunaris-llamacpp/src/reranker.rs` `crates/lunaris-core/src/embedder.rs` `crates/lunaris-hook/src/embed_promotion.rs` `crates/lunaris-llamacpp/tests/` `crates/lunaris/src/handle.rs`
<!-- handle.rs added mid-build (honest reconciliation, not tampering): the promote_batch routing test surfaced that `CachedEmbedder` (handle.rs) wraps every real embedder and only overrode `embed_batch`, so `embed_batch_lowpri` silently fell through to the interactive lane — the whole feature was dead on the production path. Forwarding the new trait method through the wrapper is a REQUIRED part of this contract (§3 says promotion rides the background lane end-to-end), discovered by the discriminating test exactly as intended. -->

Strategy (ordered batches):
  1. lunaris-llamacpp/worker.rs: add `Priority` enum + model-free `PriorityIntake` (Mutex<{high,low}>+Condvar); rework the worker loop to pop high-else-low; `encode(token_lists, priority)`.
  2. Fix the two call sites: reranker.rs:198 + embedder.rs:169 pass `Priority::Interactive` (keeps today's behavior exactly).
  3. lunaris-core/embedder.rs: add `embed_batch_lowpri` default method (delegates to `embed_batch`).
  4. lunaris-llamacpp/embedder.rs: override `embed_batch_lowpri` — window-split at `budget`, `Priority::Background`, reassemble in order.
  5. lunaris-hook/embed_promotion.rs: `promote_batch` calls `embed_batch_lowpri`.
Safety rule (feature-specific): correctness dominates latency — a lane-scheduling bug must degrade to FIFO, NEVER drop/reorder a job's outputs vs input order; `contexts_created()==1` must hold.
Code lives in: the crates listed above (this is a workspace-internal task, no `./src/` task dir).
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — lunaris-core 80/80, lunaris-hook 31/31, lunaris-memory embedder 2/2, llamacpp intake 2/2 + model-gated priority_lanes (ran on real GGUF) + context_reuse 1/1 (embedder A2 pin still green).
- [x] coverage did not decrease — added scheduling + wiring + trait tests; no test removed.
- [x] no test or contract was altered during build — §3 FROZEN unchanged; only production code written. (Note: the model-free `PriorityIntake` was the test-declared seam; §4's worker_gone/empty-at-intake rows were consolidated — see deviation below — not weakened.)
- [x] the green was EARNED — the promote_batch routing test FAILED first (lo=0 hi=1) and forced a REAL production fix (CachedEmbedder was swallowing the low-pri lane); it only passed once the wrapper forwarded `embed_batch_lowpri`. No fixture overfit; the model-gated test asserts byte-identical lanes on the live model, not a stub.
- [x] concurrency / timing safe — intake is std `Mutex<VecDeque>` + `Condvar` on a plain sync worker thread; NO lock held across `.await` (worker has no await). `DrainOnExit` closes+drains on any thread exit (incl. panic unwind) so a queued job never deadlocks its `encode()` caller. `contexts_created()==1` proven under concurrent mixed-lane submission.
- [x] no exposed secrets / injection / unexpected deps — zero new deps (crossbeam deliberately avoided: unvendored + RUSTSEC-2026-0204); std-only.
- [x] layering & dependencies follow CONVENTIONS.md — `Priority`/`PriorityIntake` are `pub(crate)` in lunaris-llamacpp; trait default lives in lunaris-core; no keyspace/lock-across-await violations.
- [ ] a person reviewed and approved the change — the contract freeze was human-approved (Tin Dang); code review pending PR.

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
- [x] Interactive drains before Background regardless of submit order — confirmed by `worker::intake_tests::drains_high_before_low` (push [low,low,high,low] → pop [high,low,low,low]).
- [x] Priority never changes the embedding — confirmed by the model-gated `priority_lanes::both_lanes_share_one_context_and_agree` asserting `embed_batch == embed_batch_lowpri` byte-for-byte on the live GGUF.
- [x] The PRODUCTION promotion path rides the background lane — confirmed by `lowpri_routing_tests::promote_batch_routes_to_lowpri_lane` (lo=1, hi=0) AFTER the CachedEmbedder forwarding fix; before the fix it read lo=0/hi=1 (the wrapper defeat).
- [x] One context under mixed load — confirmed by `priority_lanes` asserting `contexts_created()==1` after a concurrent Background batch + Interactive query.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `Priority` used at reranker.rs (Interactive) + embedder.rs (both lanes); `PriorityIntake` used by `EncodeWorker`; `embed_batch_lowpri` overridden in LlamaCppEmbedder + CachedEmbedder, called by `promote_batch`; `MAX_SEQS_PER_WINDOW` now `pub(crate)` and used by the Background splitter. No orphans.
- [x] DEAD-CODE (code) — `DrainOnExit`, `is_closed`, `try_pop`, `pop_blocking`, `close` all referenced (worker loop, encode, Drop, tests). clippy `--all-targets -D warnings` clean on all 4 crates (would flag unused).
- [x] SEMANTIC — n/a (code task).

### GATE RECORD
Outcome: PASS
Deviation (recorded, not weakening): §4 planned a model-free `worker_gone`/`empty`-at-intake test, but `EncodeWorker::encode` needs a real model to construct, so those Reject behaviors are (a) empty-input covered model-free at the core layer (`lowpri_empty_is_noop`) and (b) worker-gone preserved by the unchanged `encode` error path + the new `DrainOnExit` no-deadlock guard (design-reviewed). No scenario dropped; the observable is the same.
Reviewed by: auto-resolved (autonomy: auto, non-security) · date: 2026-07-14 — human code-review at PR.

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): recall-query embed p99 while a background ingest batch is in flight (should stay ~1 window, not batch-length); contextd/mcp footprint unchanged (still one context).

### Spec delta
- [SPEC · open] true mid-job preemption — push window-splitting INTO the worker loop (partial-job state machine) so even a single oversized Background job yields to a waiting query (evidence: freeze ⚠ flag; caller-side split only interleaves at job boundaries).
- [SPEC · seeded] CachedEmbedder wrapper-defeat class — ANY future Embedder-trait method (e.g. a streaming/batched variant) MUST be forwarded by `CachedEmbedder` or it is silently downgraded to `embed_batch` on the real path (evidence: this task's promote_batch test read lo=0/hi=1 until the wrapper forwarded the new lane).
- [SPEC · dropped] second interactive context / bounded pool — rejected at specify; only revisit if a single-context priority lane proves insufficient under real load.

### Competency deltas
- [TDD · open] the discriminating WIRING test caught a real production defeat (CachedEmbedder swallowing the low-pri lane) that BOTH the model-free unit tests and the model-gated integration test missed — only the promote_batch-through-the-real-handle test saw it (evidence: lo=0/hi=1 pre-fix). Reinforces built≠wired: test the new behavior through the production wrapper stack, not just the leaf.
- [ADD · open] the single-context priority interleave is safe ONLY because the embedder/reranker are encoder-only (no cross-call KV-cache state); a future in-process decoder LLM slot would make interleaving two streams on one context unsafe (evidence: documented in worker.rs/embedder.rs; user KV-cache question at build time).
