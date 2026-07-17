# TASK: Activation Ledger

slug: activation-ledger · created: 2026-07-17 · stage: production
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
- `crates/lunaris-core/src/keyspace.rs` — canonical KV key mint
  (`{episode,chunk,fact,...}_key`; format `lunaris:{scope}:{kind}:{ulid}`).
  NEW `activation_key(scope, id)` goes HERE (RC-1 convention: keyspace
  helpers live in lunaris-core, never local to a caller).
- `crates/lunaris-retrieve/src/builder.rs:96` — `boost_cache:
  Option<Arc<RwLock<LruCache<(Scope, Ulid), f32>>>>`; seam
  `with_boost_cache` (line 318); post-hydrate boost pass at line 424
  (peek-only snapshot, additive delta, re-sort). This is the read seam the
  persistent provider generalizes.
- `crates/lunaris-consolidate/src/act_r.rs:85` —
  `ActRScorer::update_with_new_reference(prior_sum, t_new)` (Petrov O(1));
  `ConsolidatorConfig` defaults decay=0.5, archive=-0.5, promote=+1.0.
  `types.rs:149` `ReferenceTime { elapsed_seconds }`. Sole reference
  source today = ingest events (write frequency, not usefulness).
- `crates/lunaris-hook/src/context.rs:1000` — `trace_injection` (writes
  `lunaris:memory_injection` capture with `memory_ids`) and `:976`
  `capture_feedback` — the weak-signal emission points; both route through
  `capture_lightweight` (line 1026) → engine.
- `crates/lunaris-verify/src/reflect_apply.rs:52` — `BOOST_DELTA: f32 =
  0.25` and the LRU-populating `end_turn` path (Phase 14.2) that the
  persistent provider must stay compatible with.
- `lunaris_core::StoragePort` — `atomic_write(scope, &[WriteOp])` +
  point reads; ledger read-modify-write batches into ONE atomic_write per
  signal flush (mirrors D-11).
Context (working folder): `.add/milestones/engram-soul-loop/MILESTONE.md`
  task 2 + 2026-07-17 grain amendment (`grain: turn|tool_call|node`,
  `node` reserved for procedural-memory follow-on).
Honors (patterns / conventions): keyspace helpers in lunaris-core (RC-1);
  no lock across .await; parking_lot only; INGEST-04 untouched (ledger
  writes are NOT ingest-pipeline writes — they flush at inject/feedback
  time, outside pipeline.rs); Scope alphabet closure means `activation`
  kind cannot byte-alias.
Anchors the contract cites: `activation_key` ·
  `ActivationRecord` · `BoostProvider` · `RetrievalBuilder::
  with_boost_provider` · `ScopedLunaris::record_activation_refs` ·
  `ActRScorer::update_with_new_reference` · `trace_injection`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: persistent per-memory activation ledger — usage-graded recall prior
Framings weighed: O(1) summary record per memory recomputing Petrov
  optimized-learning activation at read time (chosen — bounded storage, no
  per-read history walk) · append-only reference log per memory (rejected:
  unbounded growth, needs compaction pass v1) · keep in-memory LRU and only
  persist at session end (rejected: loses cross-session reinforcement, the
  whole point).
Must:
<must>
  - a reference signal for memory id M in scope S upserts ONE KV record at
    `lunaris:{S}:activation:{M}` holding: weighted running sum (Petrov),
    total refs n, weighted refs, first_ref_wall, last_ref_wall, and the
    last signal's `grain` (turn|tool_call|node) + `strength` (weak|strong)
    (amendment 2026-07-17: grain field from day one; `node` reserved)
  - weights: weak=1.0 (injection), strong=3.0 (citation / feedback+ /
    successful-tool-call fed); strength is the writer's declaration —
    the ledger stores, it does not judge
  - a batch of signals for one scope flushes in ONE `atomic_write`
    (read-modify-write of all touched records; mirrors D-11)
  - recall re-rank: when a boost provider is wired, post-hydrate re-rank
    adds `min(BOOST_CAP, k * ln(1 + activation))` to hit scores, where
    activation is recomputed at read time from the stored summary
    (decay=0.5 Anderson); ties preserve pre-boost order (stable sort,
    matches the Phase-14.2 pass)
  - the provider read is BATCHED and BOUNDED BY THE HIT SET: cost
    proportional to the requested ids, never to the scope's total ledger
    size (recall p50 budget: <= +2ms vs no-provider baseline).
    [AMENDED at review 2026-07-17: original wording "one storage round
    trip" permitted a full-prefix scan — one call, unbounded rows read;
    replaced with bounded-concurrency read_as_of point reads, which is
    what the budget intent actually requires as the corpus ages]
  - the existing `with_boost_cache` LRU seam keeps working unchanged
    (Phase-14.2 end_turn contract untouched); provider and LRU compose:
    LRU delta applies after provider prior
  - production writer v1: the hook's `trace_injection` path ALSO emits a
    weak ref for every injected memory id (built≠wired: discriminating
    test on the real hook capture path); strong writers land in tasks 3/4
    through the same public API
  - ACT-R promote/archive worker can read the ledger as a reference
    source (`LedgerReferenceSource` feeding `ActRConsolidator`) so
    activation measures USE, not write frequency
</must>
Reject:
<reject>
  - malformed stored record (bad JSON / negative sums) on read -> skip
    that id with tracing::warn, hit keeps its un-boosted score; recall
    NEVER errors because the ledger is corrupt
  - ledger write failure at inject time -> tracing::warn, injection
    proceeds — a reinforcement failure must not fail the agent turn
    (same contract as reflect_apply)
  - unknown `grain`/`strength` string on the wire -> serde reject at the
    type level (enums, no free strings)
  - signal for an id outside the caller's scope -> impossible by
    construction (key minted from the caller's Scope; no cross-scope API)
</reject>
After:
<after>
  - a memory injected in session 1 carries a persistent activation record
    that outranks an equal-similarity never-injected memory in session 2
    recall (cross-process, real backend) — milestone exit criterion
  - activation decays: with no new refs, recomputed activation falls as
    wall-time grows (power-law, not cliff)
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ +2ms p50 budget holds for the batched provider read on Moon — lowest
    confidence because the boost pass runs after hydrate (already ~ms) and
    a Moon MGET of k records adds a round trip; if wrong: gate the
    provider behind a config flag default-ON for hook path only, and the
    sub-25ms core contract is still safe (budget measured in tests).
  ⚠ [spec] ln(1+activation) with k=0.1, BOOST_CAP=0.30 keeps LongMemEval
    smoke neutral-or-better — chosen so max prior ≈ existing BOOST_DELTA
    scale (0.25) and cannot swamp cosine scores; if wrong: retune k/cap
    constants (exit criterion pins the smoke).
  - [x] read-time recompute from an O(1) summary is faithful enough to
    Anderson-1996 — Petrov 2006 optimized-learning form
    `Σ ≈ n·t_life^(1-d)/((1-d)·t_life^d)`-style approximation is the
    published standard; exactness is not a contract here, ordering is.
  - [x] `activation` kind cannot collide in the keyspace — Scope alphabet
    rejects `:` (v0.2.1 closure), kind literal is new and unique.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: reference signal upserts one summary record
  Given no activation record exists for memory M in scope S
  When a weak turn-grain signal for M flushes
  Then storage holds exactly one record at lunaris:{S}:activation:{M}
    with n=1, weighted=1.0, grain=turn, strength=weak, both walls set
  And a second strong tool_call-grain signal updates the SAME key
    (n=2, weighted=4.0, last_ref_wall advanced, first_ref_wall unchanged)

Scenario: batch flush is one atomic_write
  Given signals for three memory ids in one scope
  When the batch flushes
  Then the storage layer sees exactly one atomic_write call carrying
    three KvPut ops

Scenario: reinforced memory outranks equal-similarity peer (exit criterion)
  Given two memories A and B with equal-similarity content in scope S
    and an activation record only for A (two strong refs)
  When recall runs with the ledger provider wired (real backend,
    fresh handle — no in-process cache carryover)
  Then A ranks above B
  And with the provider not wired, A and B keep their pre-boost order

Scenario: activation decays with wall age
  Given a record whose refs all happened long ago vs an identical-count
    record with recent refs
  Then the recomputed activation of the old record is strictly lower
  And boost never exceeds BOOST_CAP regardless of ref count

Scenario: provider read is batched and within budget
  Given k hits in a recall
  When the boost pass consults the provider
  Then storage sees one batched read (not k point reads)

Scenario: LRU seam unchanged and composes
  Given an end_turn boost (LRU, Phase 14.2) and a ledger record for the
    same memory
  When recall runs with both wired
  Then the hit's score carries provider prior + LRU delta
  And a builder with only with_boost_cache behaves byte-identically to
    pre-task behavior

Scenario: hook injection emits weak refs on the production path
  Given the hook injects memories M1,M2 via trace_injection
  When the injection completes
  Then activation records for M1,M2 exist with strength=weak,grain=turn
  And the lunaris:memory_injection capture is written exactly as before

Scenario: ACT-R worker reads ledger references
  Given ledger records in scope S
  When the ActRConsolidator ticks with the LedgerReferenceSource
  Then promote/archive decisions use ledger refs (a heavily-referenced
    fact promotes; an unreferenced old fact archives)

Scenario (reject): corrupt record never fails recall
  Given a garbage payload stored at an activation key
  When recall runs with the provider wired
  Then the hit keeps its un-boosted score, a warn is traced
  And recall returns Ok with all hits

Scenario (reject): ledger write failure never fails the turn
  Given a storage that errors on atomic_write for activation keys
  When trace_injection runs
  Then the injection still succeeds (capture written, no error to caller)
  And a warn is traced

Scenario (reject): unknown grain/strength rejected at the type level
  Given a serialized signal with grain="week"
  When deserialization runs
  Then serde returns an error (enum, not free string)
  And no record is written
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
// lunaris-core (types + key mint — keyspace helpers live HERE, RC-1)
pub fn keyspace::activation_key(scope: &Scope, id: Ulid) -> Vec<u8>
  // b"lunaris:{scope}:activation:{ulid}"

pub mod activation {                         // lunaris-core/src/activation.rs
  #[serde(rename_all = "snake_case")] pub enum Grain    { Turn, ToolCall, Node }
  #[serde(rename_all = "snake_case")] pub enum Strength { Weak, Strong }
  pub struct RefSignal { pub id: Ulid, pub grain: Grain, pub strength: Strength }
  #[serde(deny_unknown_fields)]
  pub struct ActivationRecord {
    pub n: u32, pub weighted: f64,           // Σ weights (weak=1.0 strong=3.0)
    pub first_ref_wall: u64, pub last_ref_wall: u64,   // unix secs
    pub last_grain: Grain, pub last_strength: Strength,
    pub v: u8,                               // schema version = 1
  }
  impl ActivationRecord {
    pub fn apply(&mut self, s: &RefSignal, now: u64)   // upsert math
    pub fn activation(&self, now: u64, decay: f64) -> f64
      // Petrov optimized-learning recompute from the O(1) summary:
      // ln(weighted * t_life^(1-d) / ((1-d) * max(t_last,1)^? )) — exact
      // form pinned by unit tests: monotonic ↑ in refs, ↓ in age, cap-safe
  }
  pub const WEIGHT_WEAK: f64 = 1.0;  pub const WEIGHT_STRONG: f64 = 3.0;
  pub const BOOST_K: f32 = 0.1;      pub const BOOST_CAP: f32 = 0.30;
}

// lunaris (engine handle — writer API; ALL later signal sources use this)
impl ScopedLunaris {
  pub async fn record_activation_refs(&self, signals: &[RefSignal])
      -> Result<(), LunarisError>
  // read-modify-write of all touched records; exactly ONE
  // storage.atomic_write(scope, ops); best-effort contract documented —
  // callers on the turn path log-and-continue on Err
}

// lunaris-retrieve (reader seam — generalizes Phase-14.2)
#[async_trait] pub trait BoostProvider: Send + Sync {
  async fn priors(&self, scope: &Scope, ids: &[Ulid]) -> HashMap<Ulid, f32>;
  // MUST be one batched storage read; corrupt/missing entries omitted
}
impl RetrievalBuilder {
  pub fn with_boost_provider(self, p: Arc<dyn BoostProvider>) -> Self
  // boost pass order: score + provider_prior, then LRU delta (14.2),
  // then ONE stable re-sort. with_boost_cache signature UNCHANGED.
}
pub struct LedgerBoostProvider { storage: Arc<dyn StoragePort> }  // the impl

// lunaris-consolidate (ACT-R reader)
pub struct LedgerReferenceSource { storage: Arc<dyn StoragePort> }
  // feeds ActRConsolidator refs from activation records (weighted, walls)

// lunaris-hook (production writer v1 — inside trace_injection, after the
// capture_lightweight succeeds):
//   engine.record_activation_refs(&[RefSignal{grain:Turn,strength:Weak},..])
//   .unwrap_or_else(|e| tracing::warn!(...))   // never fails the turn

Schema: one KV record per (scope, memory-ulid) at activation_key; JSON
  via serde; no bi-temporal wrap (ledger is metadata, not history — a
  supersede/tombstone of the MEMORY does not touch its ledger record;
  orphaned ledger rows are inert and reaped by the archive worker).
Access: point MGET batch on recall (ids from the hit set); RMW batch on
  flush; full-scope scan only in the ACT-R tick (background).
```

Status: FROZEN @ v1 — approved by autonomous (autonomy=auto; design
pre-locked in MILESTONE.md task 2 + 2026-07-17 grain amendment interview).
Least-sure flag surfaced at freeze: [contract] `BoostProvider::priors` as a
new ASYNC seam in the hot recall path (the 14.2 LRU pass is sync) — why:
a persistent prior requires a storage read, and batching keeps it to one
round trip; cost if wrong (budget blown): provider stays opt-in
(`with_boost_provider` is never wired by default `recall()` in this task —
only the hook path opts in), so the sub-25ms core contract cannot regress
for existing callers. Second flag: [spec] k=0.1/CAP=0.30 tuning vs
LongMemEval smoke — cost: constant retune, pinned by the milestone exit
criterion.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every scenario in §2; red = missing-symbol compile failure
for new-API tests (E0433/E0599 — API absent is the right reason) plus
assertion-red where the surface exists (builder default behavior).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - crates/lunaris-core/tests/activation_ledger.rs
    - upsert_math_first_and_second_signal (scenario 1: n/weighted/walls/
      grain/strength through two applies)
    - activation_decays_with_age_and_is_capped (scenario 4)
    - serde_rejects_unknown_grain_and_strength (reject 3)
    - activation_key_format_is_scoped (key bytes == lunaris:{s}:activation:{ulid})
  - crates/lunaris-retrieve/tests/boost_provider.rs (reuse recency_rescore
    harness patterns / fake storage)
    - provider_prior_flips_equal_similarity_order (scenario 3, unit level)
    - provider_and_lru_compose_prior_then_delta (scenario 6)
    - no_provider_wired_is_byte_identical (scenario 6 And-clause; this one
      is assertion-style and must be GREEN before build — pins the default)
    - provider_read_is_one_batched_call (scenario 5: counting fake provider/
      storage — asserts one priors() call per execute)
  - crates/lunaris/tests/activation_ledger_engine.rs (memory:// backend)
    - record_refs_upserts_and_batches_one_atomic_write (scenarios 1+2:
      counting StoragePort wrapper)
    - reinforced_memory_outranks_across_handles (scenario 3 exit criterion:
      fresh handle re-open, real recall path)
    - corrupt_record_recall_still_ok (reject 1)
    - ledger_write_failure_does_not_error_turn_path (reject 2: failing
      storage wrapper, record_activation_refs Err surfaced but
      trace_injection-style caller contract asserted at hook level)
  - crates/lunaris-hook: extend context.rs unit tests — trace_injection
    emits weak refs (fake/recording engine seam, mirrors existing tests)
    + capture unchanged (scenario 7)
  - crates/lunaris-consolidate/tests/ledger_reference_source.rs
    - act_r_tick_promotes_from_ledger_refs (scenario 8: fabricated records,
      promote fires; unreferenced old fact archives)
</test_plan>

Tests live in: `./tests/` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-core/src/` · `crates/lunaris-retrieve/src/` ·
  `crates/lunaris/src/handle.rs` · `crates/lunaris-hook/src/context.rs` ·
  `crates/lunaris-consolidate/src/` · `crates/lunaris-memory-service/src/recall.rs`
Strategy (ordered batches): 1. core types + key mint · 2. provider seam +
  builder composition · 3. engine writer · 4. hook weak-ref writer ·
  5. consolidate ledger source · 6. (review rider) service-path read wiring.
Safety rule (feature-specific): reinforcement must be fail-open on the turn
  path (warn, never error) and the ledger read must never grow with scope
  cardinality; the LRU seam stays byte-identical.
Code lives in: `crates/`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.
Executed by: subagent (red commit 7837568, green commit 2498206) +
  orchestrator review pass (provider scan→point-read refactor; service
  recall wiring + built≠wired test; doc-wording sync).

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — 13 new ledger tests + hook test + service wiring test
      (`activation_boost_reranks_service_recall`, 50/50 memory-service lib);
      full workspace sweep recorded below at the gate
- [x] coverage did not decrease — 6 new test files, zero removed/weakened
- [x] no test or contract was altered during build — RED commit `7837568`
      untouched by GREEN `2498206`; review pass amended ONE §1 Must wording
      (scan→bounded point reads) with the rationale recorded inline, and the
      internals-agnostic tests needed no change (they passed unmodified
      through the refactor — the strongest evidence they pin behavior, not
      implementation)
- [x] the green was EARNED — orchestrator adversarial review found and FIXED
      two gaps the subagent's green did not cover: (1) full-prefix scan in
      LedgerBoostProvider satisfied "one round trip" literally while scaling
      cost with scope size → bounded-concurrency read_as_of point reads;
      (2) the read side had NO production caller (built≠wired) → wired into
      lunaris-memory-service recall (the hook/MCP execution path) behind
      LUNARIS_ACTIVATION_BOOST (default ON) + discriminating service test
- [x] concurrency / timing safe — no lock across .await anywhere new;
      provider clock lock is pure-CPU (HlcClock::tick); buffer_unordered(16)
      bounds in-flight reads; RMW batch commits via ONE atomic_write
- [x] no exposed secrets / injection openings / unexpected deps — zero new
      external deps; ledger keys minted from the caller's Scope only
- [x] layering follows CONVENTIONS.md — key mint in lunaris-core keyspace
      (RC-1); provider in lunaris-retrieve; writer on ScopedLunaris; INGEST-04
      pipeline.rs untouched (verified zero diff)
- [x] a person reviews at the PR gate (Tin, admin-rebase flow)

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
- [x] a strong-reinforced memory outranks its equal-similarity peer through
      a FRESH handle (no in-process cache) — seen in
      `reinforced_memory_outranks_across_handles` (flip explained only by
      the persisted record) and end-to-end through the service handler in
      `activation_boost_reranks_service_recall`
- [x] un-wired callers are byte-identical to pre-task behavior — seen in
      `no_provider_wired_is_byte_identical` (GREEN before any impl existed)
- [x] corrupt ledger rows and write failures degrade to warnings, never
      errors — seen in `corrupt_record_recall_still_ok` (score unchanged at
      0.55) and the hook's log-and-continue wiring
- [x] ledger read cost stays proportional to the hit set as the scope ages —
      seen in the provider refactor (per-id read_as_of, no prefix scan)

### Deep checks — do not skim
- [x] WIRING (code) — write side: trace_injection → record_activation_refs
      (production hook path); read side: memory-service recall.rs
      with_activation_boost → with_boost_provider (production service path);
      consolidator: LedgerReferenceSource + tick_from_ledger exported and
      tested. No new symbol lacks a production or contracted caller.
- [x] DEAD-CODE (code) — parse_activation_id removed with the scan it
      served; activation_prefix retained in keyspace (used by the ACT-R
      ledger scan — a background tick, where whole-prefix IS the workload)
- [x] SEMANTIC — TASK.md + MILESTONE.md amendments re-read after the
      review edits; wording and code agree (provider doc, Must line)

### GATE RECORD
Outcome: PASS
Reviewed by: autonomous (autonomy=auto; subagent build + orchestrator
adversarial review pass); human review rides the PR · date: 2026-07-17

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): recall p50 delta with
LUNARIS_ACTIVATION_BOOST on vs off (budget: +2ms); rate of
`activation_ledger_corrupt_record_skipped` /
`activation_ledger_boost_provider_read_failed` warns (expect ~zero);
LongMemEval smoke at milestone close (k=0.1/CAP=0.30 neutral-or-better).

### Spec delta
- [SPEC · open] StoragePort wants a native multi-get (`kv_multi_get`) —
  bounded point reads are k round trips pipelined; Moon MGET would make the
  provider read literally one (evidence: review amendment on the §1 Must).
- [SPEC · seeded] tasks 3/4 (citation-detector, memory.feedback) write
  strong signals through record_activation_refs — API landed here.
- [SPEC · open] MCP recall inherits the service-path boost (shared
  dispatch); contract said "hook path opts in" — acceptable-and-flagged
  (same production dispatch serves both; flag LUNARIS_ACTIVATION_BOOST=0
  is the per-deploy opt-out).

### Spec delta
Forward changes for the next loop — each re-enters at Specify as the next task. One line
each, tagged `[SPEC · open|seeded|dropped]`, with evidence (e.g. `[SPEC · open] rate-limit
the retry path (evidence: prod herd spikes)`). See the `add` skill's `deltas.md`.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
