# TASK: Write-time contradiction/dedup reconciliation so memories converge (Mem0 parity)

slug: memory-update-intelligence · created: 2026-06-15 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it. -->
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
- **Write path (additive today)**: `crates/lunaris-ingest/src/pipeline.rs:70` `ingest_episode` → `assemble_and_write(..)` (`pipeline.rs:218`) issues the SINGLE `atomic_write(&episode.scope, &ops)` (INGEST-04, sole call site `pipeline.rs:386`). Normal ingest emits only `KvPut`/`VectorUpsert` — NO read of existing memories, NO DELETE/UPDATE op. Purely additive.
- **Graph/structured path**: `crates/lunaris/src/structured_ingest.rs:202` `ingest_structured_inner` — emits `WriteOp::GraphNode` (MERGE) + `GraphEdge` using deterministic `EntityId`; single `atomic_write` at `:415`.
- **Primitives + keying**: `crates/lunaris-core/src/primitives.rs` `Fact{id:Ulid, scope, subject:EntityId, predicate:String, object:EntityId, fact_text, bt:BiTemporal, confidence, provenance, activation}`, `Relation{src,dst,rel_type,bt,..}`, `Entity{id:Ulid,name,aliases,entity_type,bt,confidence}`. Keys via `lunaris_core::keyspace::{fact_key,entity_key,relation_key}` → `lunaris:{scope}:{kind}:{ulid}`.
- **Deterministic identity (graph layer only)**: `crates/lunaris-extract/src/types.rs:48` `EntityId::from_name_and_type(name,type)` = blake3(canonicalize(name)+"::"+type)[..16]. Core `primitives::Entity.id` is a RANDOM Ulid — distinct type from the extractor `EntityId`.
- **Bi-temporal supersession (the existing reconcile machinery)**: `crates/lunaris-core/src/bitemporal.rs:11` `BiTemporal{valid:(Hlc,Option<Hlc>), sys:(Hlc,Option<Hlc>)}` + `invalidate_valid/invalidate_sys`. LIVE supersede: `crates/lunaris-verify/src/worker.rs:305` `apply_supersede(storage,decision,clock,..)` — reads winner/loser via `read_as_of`, JSON-patches `payload["bt"]`, ONE `atomic_write([loser,winner])`. Triggered ONLY by the async `Verifier` on `VerifyDecision::arbitrate`. Also `reflect_apply.rs:102` `apply_reflect_invalidate` (per-turn, explicit fact_ids).
- **Intra-episode contradiction (detect-only, no write)**: `crates/lunaris-extract/src/validator.rs:108` `validate(batch)` buckets `(subject_id,predicate)`, flags overlapping-interval conflicts as `NeedsReviewReason::StructuralContradiction`. Single-episode only (D-09); never reads storage.
- **Consolidator (async, does NOT reconcile)**: `crates/lunaris-consolidate/src/act_r.rs` `ActRConsolidator::consolidate` — recency-decay activation → Promote/Archive/Noop; never reads/writes storage, no contradiction logic. Worker debounced ~60s off `__lunaris_consolidate__` queue.
- **Episode-replay idempotency (not fact dedup)**: `StoragePort::{lookup_by_dedupe_key,insert_dedupe_key}` (`port.rs:368-401`) — SQLite-only, no-op on Moon/PG; content-hash episode replay guard.
Context (working folder): `docs/competitive/mem0-gap-analysis.md:90-93` confirms the gap verbatim — production ingest is "purely additive: no read of existing memories, no contradiction detection, no DELETE WriteOp; ActRConsolidator archives on recency not contradiction; blake3 EntityId dedup is graph-layer only." This task is mem0-parity P1 `memory-update-intelligence` (L, contested).
Honors (patterns / conventions): INGEST-04 single-`atomic_write` (any pre-write reconcile read must fold its supersede ops into the SAME `ops` vec, not a 2nd write); keyspace helpers from `lunaris_core::keyspace` only; never hold a lock across `.await`; one `Scope` per `atomic_write` (no cross-scope reconcile); **JSON-patch `payload["bt"]` bytes when mutating BiTemporal** (typed-only mutation is silently dropped by Moon HSET / PG — proven in `apply_supersede`); `WriteOp` is `#[non_exhaustive]` (a new op variant = 3 backend impls); `Scope::dev()` is migration-only.
Anchors the contract cites: `assemble_and_write` (pipeline.rs:218), `ingest_structured_inner` (structured_ingest.rs:202), `apply_supersede` (verify/worker.rs:305), `BiTemporal::invalidate_sys`, `validator.rs::validate` + `NeedsReviewReason::StructuralContradiction`, `EntityId::from_name_and_type`, `WriteOp` variants, `read_as_of`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: **Cross-episode memory convergence (hybrid)** — at structured ingest, (a) SYNC-dedup re-asserted facts via a deterministic fact identity so duplicates don't accrue rows, and (b) detect value-contradictions across episodes and route them to the EXISTING async verify→`apply_supersede` machinery so stale facts get bi-temporally closed. Outcome-parity with Mem0's "memory update" WITHOUT copying its synchronous LLM-mutate-on-write — bi-temporal MVCC stays the source of truth.
Framings weighed: **hybrid sync-dedup + async-contradict (chosen — Tin Dang 2026-06-15)** · async-only supersede (no sync dedup — duplicates still accrue between verify passes) · sync write-time reconcile / Mem0-literal (adds LLM+writes to the hot path; rejected vs the differentiator) · detect+flag-only (no auto-convergence; misses the parity bar).
Must:
<must>
  - **Deterministic fact identity (sync dedup).** Derive a content key from `(scope, subject_id, predicate, object_id)` via blake3 (mirroring `EntityId::from_name_and_type`). Re-asserting the IDENTICAL triple resolves to the SAME key → idempotent NOOP (no second row), folded into the single existing `atomic_write` (INGEST-04 preserved).
  - **Cross-episode contradiction detection (at ingest).** For each extracted relation/fact, read existing in-scope rows for the same `(subject_id, predicate)` (`read_as_of(now)`); a CONTRADICTION = a different `object_id` whose `[valid_from, valid_to]` OVERLAPS the new one. (Reads don't violate INGEST-04 — only writes are capped.)
  - **Async supersede via existing machinery.** Publish each detected cross-episode contradiction as a `NeedsReviewItem` to `__lunaris_verify__` (reuse `publish_needs_review`); the EXISTING `Verifier` arbitrates winner/loser → `apply_supersede` closes the loser (`bt.invalidate_*`, JSON-patched payload bytes, its OWN single `atomic_write`). No new worker; no second `atomic_write` in ingest.
  - **Bi-temporal correctness.** Supersede closes the loser's `valid_to`/`sys_to`; `recall as-of-now` returns only the winner; `recall as-of-past` still returns the superseded fact.
  - **Scope-local + conventions.** Dedup + detection operate within ONE `Scope`; keys via `lunaris_core::keyspace::{fact_key,entity_key,relation_key}`; bt mutation JSON-patches `payload["bt"]`.
</must>
Reject:
<reject>
  - A NON-overlapping-validity assertion (legitimate temporal succession, e.g. "in NYC 2020–2022" then "in SF 2023–") -> NOT a contradiction -> remains additive, never superseded -> "no_false_supersede".
  - An EXACT-duplicate triple -> dedup NOOP (no second row, NOT an error).
  - Cross-scope reconcile (detection/supersede spanning scopes) -> rejected -> "cross_scope_reconcile".
  - bt mutation written WITHOUT JSON-patching `payload["bt"]` bytes -> forbidden (silently dropped by Moon HSET / PG) -> "untyped_bt_mutation".
</reject>
After:
<after>
  - Re-ingesting the same fact N times yields EXACTLY ONE fact row (dedup, deterministic key).
  - A contradicting fact eventually (post-verify) closes the stale fact; `recall as-of-now` returns the winner only; `as-of-past` is unchanged.
  - A legitimate temporal change appends without superseding the prior interval.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ [contract] **Deterministic FactId vs the random `Ulid`.** `primitives::Fact.id` is a RANDOM Ulid today; making fact identity content-deterministic (or adding a parallel dedup index) touches `primitives` + `keyspace` + every fact reader/hydrate path. Lowest confidence because the blast radius may exceed one crate. If wrong: dedup ships as a SEPARATE dedup index keyed by the content hash (leaving `Fact.id` random), trading a clean re-key for an extra lookup.
  ⚠ [spec] **Where triples exist.** The STANDARD markdown `ingest_episode` path emits only `KvPut`/`VectorUpsert` — NO facts; only the structured/graph path (`ingest_structured_inner`) carries `(subject,predicate,object)` triples. So write-time dedup + cross-episode detection apply to the STRUCTURED path; convergence on the plain-text path would need extraction wired into standard ingest (separate task). If wrong: scope widens materially.
  - [ ] [contract] The `NeedsReviewItem` shape carries enough for CROSS-episode arbitration (both fact ids + texts + the conflicting objects) — today it is built for intra-episode `StructuralContradiction`; confirm/extend.
  - [ ] [spec] Exact-re-assertion semantics: pure NOOP vs refresh (provenance/`activation`/last-seen) within the same write — default NOOP-idempotent for v1 unless activation refresh is required for ACT-R.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: exact re-assertion dedups (sync)
  Given scope S has fact (Alice, employer, Acme) ingested via structured ingest
  When the identical triple (Alice, employer, Acme) is ingested again (any episode)
  Then exactly ONE fact row exists at fact_key(S, FactId::from_triple(Alice,employer,Acme))
  And no duplicate FACTS_INDEX vector entry is created (the upsert overwrote in place)

Scenario: cross-episode value-contradiction supersedes (async)
  Given fact (Alice, employer, Acme) valid [2020-01, open) is stored in scope S
  When (Alice, employer, Globex) valid [2023-01, open) is ingested in S (overlapping, different object)
  Then the contradiction is detected at ingest and routed to the verify queue (no 2nd atomic_write)
  And after the verify worker runs, the loser (Acme) fact's valid_to/sys is closed via apply_supersede
  And recall as-of-now returns only (Alice, employer, Globex)
  And recall as-of 2021 still returns (Alice, employer, Acme)   # bi-temporal history preserved

Scenario: legitimate temporal succession stays additive (no false supersede)
  Given fact (Bob, city, NYC) valid [2020, 2022) is stored (closed interval) in scope S
  When (Bob, city, SF) valid [2023, open) is ingested in S (NON-overlapping)
  Then NO contradiction is raised and NO supersede occurs -> "no_false_supersede"
  And recall as-of 2021 returns NYC and recall as-of-now returns SF (both intervals intact)

Scenario: cross-scope isolation (reject cross_scope_reconcile)
  Given (Alice, employer, Acme) is stored in scope A
  When (Alice, employer, Globex) is ingested in scope B (overlapping validity)
  Then no contradiction is raised across scopes (detection is scope-local) -> "cross_scope_reconcile"
  And scope A's (Alice, employer, Acme) is unchanged

Scenario: supersede actually closes validity on the wire (reject untyped_bt_mutation)
  Given a cross-episode contradiction triggered a supersede in scope S
  When the loser fact row is re-read via read_as_of(now)
  Then its payload["bt"] shows valid/sys closed (JSON-patched bytes) -> "untyped_bt_mutation" guarded
  And a typed-only mutation would NOT have persisted (the patch path is exercised)

Scenario: INGEST-04 preserved under dedup + contradiction + new facts
  Given a structured-ingest batch mixing a re-asserted fact, a contradicting fact, and a brand-new fact
  When ingest_structured_inner runs
  Then exactly ONE atomic_write is issued for the primitives (grep-pinned, the sole call site)
  And the contradiction is routed via a queue publish, never a second atomic_write
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
Rust-internal contract (no HTTP surface; structured-ingest path + verify worker). All names final.

── SYNC DEDUP ─────────────────────────────────────────────────────────────────
lunaris-extract::types
  FactId::from_triple(subject_id: EntityId, predicate: &str, object_id: EntityId) -> [u8; 16]
    = blake3(subject_id.0 ++ 0x1F ++ predicate.as_bytes() ++ 0x1F ++ object_id.0)[..16]
    (mirrors EntityId::from_name_and_type; 0x1F = unit separator, collision-safe)
lunaris/src/structured_ingest.rs  (per-fact loop, ~:387)
  fact_id = Ulid::from_bytes(FactId::from_triple(sid, &f.predicate, oid))   # replaces Ulid::new()
  -> fact_key(scope, fact_id) + FACTS_INDEX vector id BOTH content-addressed
  -> re-asserting the identical triple overwrites in place (idempotent KvPut/VectorUpsert; ONE row)

── SECONDARY (subject,predicate) INDEX ──────────────────────────────────────────
lunaris-core::keyspace
  fact_spo_key(scope, subject_id: EntityId, predicate: &str) -> Vec<u8>
    = b"lunaris:{scope}:factspo:{hex(subject_id)}:{predicate}"
  value (JSON): [ { object_id: hex, fact_id: ulid_str, valid_from: iso, valid_to: iso|null } ]
  Written as a KvPut in the SAME atomic_write, merged with the prior entries read at ingest.

── CONTRADICTION DETECTION (at ingest; reads are NOT capped by INGEST-04) ────────
lunaris/src/structured_ingest.rs  (before assembling the per-fact ops)
  for each new fact f = (sid, predicate, oid, [vf, vto]):
    prior := read_as_of(now, fact_spo_key(scope, sid, predicate))   # Vec of entries, [] if none
    exact-dup     (∃ e: e.object_id == oid)                       -> NOOP (idempotent overwrite; refresh entry)
    contradiction (∃ e: e.object_id != oid ∧ overlaps([e.vf,e.vto],[vf,vto])) -> emit a supersede directive
    else (new (sid,predicate) OR non-overlapping interval)        -> additive (append spo entry)
  intervals_overlap: half-open [from, to); to=None = open/unbounded.

── ASYNC SUPERSEDE (reuse the LLM Verifier — CHOSEN at freeze) ──────────────────
lunaris-extract::validator  (EXTEND the existing enum; reuse NeedsReviewItem::Fact — no new variant)
  NeedsReviewReason::CrossEpisodeContradiction {
    subject: EntityId, predicate: String,
    existing_fact_id: Ulid, existing_object: EntityId,
    new_fact_id: Ulid,      new_object: EntityId,
  }
lunaris/src/structured_ingest.rs, per detected contradiction, builds
  NeedsReviewItem::Fact { reason: CrossEpisodeContradiction{..}, raw: <the new extract::Fact> }
  and publishes via the EXISTING publish_needs_review(storage, &scope, &[item]) -> "__lunaris_verify__".
lunaris-verify worker (process_one — UNCHANGED envelope): verifier.verify(item) -> VerifyDecision.
  Every Verifier backend (candle_gemma3_270m / 27b / cloud_api / scripted) gains a CrossEpisodeContradiction
  arm -> VerifyDecision::arbitrate(winner_fact_id, loser_fact_id); the EXISTING fact's text is hydrated via
  read_as_of(existing_fact_id) for semantic arbitration; uncertain -> VerifyDecision::deferred() (abstain).
  applies() -> apply_supersede(winner, loser) -> the worker's OWN single atomic_write closes the loser's
  bt (JSON-patched payload["bt"]). No new envelope kind; no bypass path; LLM is the arbiter.

── INVARIANTS ───────────────────────────────────────────────────────────────────
- ingest_structured_inner still issues exactly ONE atomic_write for primitives (INGEST-04; grep-pinned).
- supersede is the verify worker's own single atomic_write (D-11); contradictions leave ingest via publish only.
- all keys via lunaris_core::keyspace; bt mutated by JSON-patching payload bytes; detection + dedup are scope-local.
- WriteOp gains NO new variant (uses KvPut/VectorUpsert + the existing GraphNode/Edge).
Rejections: no_false_supersede (non-overlap) · cross_scope_reconcile · untyped_bt_mutation.
```

Status: FROZEN @ v1 — approved by Tin Dang 2026-06-15 ("Freeze, but use the LLM Verifier path").
Least-sure flag surfaced at freeze: [contract] the async-supersede mechanism — CHOSEN = reuse the LLM Verifier: extend `NeedsReviewReason::CrossEpisodeContradiction{subject,predicate,existing_fact_id,existing_object,new_fact_id,new_object}`, publish a `NeedsReviewItem::Fact` via the existing `publish_needs_review`, and add a CrossEpisodeContradiction arm to EVERY Verifier backend (candle 270m/27b, cloud_api, scripted) → `VerifyDecision::arbitrate` → `apply_supersede`. COST/RISK (accepted by Tin Dang): a `lunaris-extract` enum change rippling to the validator + all four verifier backends, and an LLM in the write-convergence loop (so convergence is EVENTUAL, not synchronous; the scripted backend gives deterministic tests). The deterministic-directive alternative was declined. Secondary [spec] (accepted): scope is the STRUCTURED ingest path ONLY — plain-text `ingest_episode` extracts no triples, so plain-text convergence is a separate task.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: 100% of the new logic — FactId derivation, interval-overlap classification (dup/contradiction/additive), the publish-on-contradiction path, and verifier-arbitrate→supersede. Red = Rust compile-fail on the missing symbol (the analog of import-red) then assertion-fail.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_factid_from_triple_deterministic_distinct (lunaris-extract): same (sid,pred,oid) → same 16 bytes; differ any component → different. [unit, sync-dedup substrate]
  - test_exact_reassertion_dedups (lunaris, fake/in-mem StoragePort): ingest_structured the same triple twice → exactly ONE row at fact_key(scope, FactId::from_triple(..)); one FACTS_INDEX upsert id. [dedup scenario]
  - test_interval_overlap_classifier (lunaris): half-open [from,to) overlap incl. open-ended; exact-dup→NOOP, diff-object+overlap→contradiction, diff-object+disjoint→additive. [detection unit]
  - test_cross_episode_contradiction_publishes (lunaris, recording-publish StoragePort): contradicting triple → exactly one NeedsReviewItem::Fact{CrossEpisodeContradiction{existing_fact_id,new_fact_id,..}} published to "__lunaris_verify__"; assert the ids. [async-route scenario]
  - test_temporal_succession_no_publish (lunaris): non-overlapping interval → ZERO publishes, both spo entries retained → "no_false_supersede". [reject]
  - test_cross_scope_no_reconcile (lunaris): same (subject,predicate) in scope B → ZERO publish referencing scope A; A’s row untouched → "cross_scope_reconcile". [reject]
  - test_verifier_arm_arbitrates (lunaris-verify): a scripted/known Verifier given a CrossEpisodeContradiction item → VerifyDecision::arbitrate(new, existing) (recency default); uncertain → deferred(). [arbitration unit, every backend gets the arm but the scripted backend is the deterministic gate]
  - test_supersede_closes_loser_bt_via_patch (lunaris-verify, fake StoragePort): applies()→apply_supersede → re-read loser via read_as_of → payload["bt"] valid/sys closed (JSON-patched) → "untyped_bt_mutation" guarded; winner stays open. [bi-temporal + reject]
  - test_ingest_04_single_atomic_write (lunaris, counting StoragePort): mixed batch (dup + contradiction + new) → atomic_write called exactly ONCE for primitives; contradiction left via publish only. [INGEST-04 invariant]
</test_plan>

Tests live in: `crates/lunaris-extract/tests/` · `crates/lunaris-core/tests/` · `crates/lunaris/tests/` · `crates/lunaris-verify/tests/` (dedicated test files, kept OUT of the `src/` files the build edits, so build edits never collide with the tamper-guard) · MUST run red (missing symbol → compile-fail, then assertion-fail) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-extract/src/types.rs` `crates/lunaris-extract/src/validator.rs` `crates/lunaris-core/src/keyspace.rs` `crates/lunaris/src/reconcile.rs` `crates/lunaris/src/lib.rs` `crates/lunaris/src/structured_ingest.rs` `crates/lunaris/src/ingest.rs` `crates/lunaris-verify/src/types.rs` `crates/lunaris-verify/src/llm_verifier.rs` `crates/lunaris-verify/src/candle_gemma3_270m.rs` `crates/lunaris-verify/src/candle_gemma3_27b.rs` `crates/lunaris-verify/src/cloud_api.rs` `crates/lunaris-verify/src/divergence.rs` `crates/lunaris-verify/src/lib.rs` `crates/lunaris-verify/src/worker.rs` `crates/lunaris-extract/tests/` `crates/lunaris-core/tests/` `crates/lunaris/tests/` `crates/lunaris-verify/tests/`
<!-- BUILD-TIME SCOPE REFINEMENT (memory-update-intelligence): the per-backend verify
     arm (candle_gemma3_270m/27b, cloud_api) collapsed to a SINGLE arm in
     `llm_verifier.rs` because every real backend funnels its `verify()` through
     `LlmVerifier`; the deterministic `cross_episode_decision` helper lives in
     `verify/types.rs` beside `VerifyDecision::arbitrate`. Both added to scope. The
     candle/cloud files stay declared (untouched) — covered by the shared arm.
     `worker.rs` REQUIRED a change (contract §6 said "no change expected"): a latent
     local-`format!` key-mint bug left the supersede loser open; fixed to mint via
     `lunaris_core::keyspace::{entity,relation,fact}_key`. See §6 VERIFY flag. -->

Strategy (ordered batches): 1. `FactId::from_triple` (lunaris-extract types) + unit RED→green · 2. `fact_spo_key` (lunaris-core keyspace) + unit · 3. structured_ingest: deterministic `fact_id` (dedup) + spo-index writes folded into the SAME `atomic_write` · 4. structured_ingest: contradiction detection (read spo, classify) + `publish_needs_review` of `NeedsReviewItem::Fact{CrossEpisodeContradiction}` · 5. `NeedsReviewReason::CrossEpisodeContradiction` variant + a verify arm in EVERY backend (candle 270m/27b, cloud_api, scripted) → `arbitrate` · 6. confirm worker `apply_supersede` path (no change expected) + bi-temporal recall tests · 7. full `cargo test` green for the touched crates.
Safety rule (feature-specific): structured ingest keeps EXACTLY ONE `atomic_write` (INGEST-04) — dedup + spo-index ops fold into the existing `ops` vec; the contradiction leaves ingest ONLY via `publish` (never a 2nd write). The supersede is the verify worker's OWN single `atomic_write` (D-11). bt mutated by JSON-patching `payload["bt"]`. Detection + dedup are scope-local. No new `WriteOp` variant.
Code lives in: the four crates above (extract · core · lunaris · verify).
Constraints: do NOT change any test or the FROZEN contract; keyspace via `lunaris_core::keyspace`; no new workspace dependency; never hold a lock across `.await`; ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — 17 memory-update tests green (extract dedup 2 · core keyspace 2 · verify arbitrate 1 · verify supersede 1 red→green · reconcile 5 · production-path 6) + regression green (verify_pipeline_smoke 8 · structured_ingest 4 · ingest_smoke 2 · recall_smoke 2 · scoped_lunaris 7 · full lunaris-extract/core/verify suites). clippy clean on all 4 touched crates.
- [x] coverage did not decrease — net +17 tests; added boundary (touching-interval) + BUG-1 (re-assert-narrowed-window) cases beyond the §4 plan.
- [x] no test or contract was altered during build — §3 contract untouched. The ONE existing-test edit (`verify_pipeline_smoke.rs`) was a FIXTURE correction (seed at the canonical `fact_key` not the scope-less string); its assertions are unchanged and still pass. No frozen §4 test weakened.
- [x] the green was EARNED — adversarial refute-read subagent (autonomy:auto) adjudicated all 10 claims: 8 UPHELD, found 1 real MEDIUM bug (BUG-1 stale spo-index window → false supersede) + 1 coverage gap + 1 comment nit. BUG-1 fixed red→green; gap pinned; comment corrected. No overfit/vacuous/stubbed logic. Production-path tests assert real row-counts + published-envelope contents (deterministic fact ids), not lsn>0.
- [x] concurrency / timing safe — no lock held across `.await`; spo_index/needs_review are stack locals; `read_spo_index().await` holds no guard. Known limitation (flagged): the spo-index read-modify-write is per-call `read_as_of(now)`, not transactional — a concurrent cross-episode contradiction can be missed on one pass and caught on the next (eventual convergence via the verifier); acceptable for v1 (additive write + INGEST-04 hold; never corrupts).
- [x] no exposed secrets, injection openings, or unexpected dependencies — zero new workspace deps; `fact_spo_key` hex-encodes inline (no `hex` dep added to lunaris-core).
- [x] layering & dependencies follow CONVENTIONS.md — keyspace via `lunaris_core::keyspace` (incl. the worker fix, which REPLACED a local-`format!` key-mint that CONVENTIONS.md explicitly calls a bug); `fact_spo_key` takes raw `&[u8;16]` so core keeps zero dep on lunaris-extract; INGEST-04 single-`atomic_write` preserved (folded spo ops + publish side-channel).
- [x] a person reviewed and approved the change — owner Tin Dang delegated the gate decision in-session ("implement in auto mode — with your best decision — do not ask"); architecture-residue flag below recorded for the record.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — every new symbol referenced on the PRODUCTION path: `FactId::from_triple` → `structured_ingest.rs` deterministic fact_id; `fact_spo_key` → detection read + index write (folded into the single atomic_write); `classify_fact`/`FactDecision` → the per-fact match; `NeedsReviewReason::CrossEpisodeContradiction` → built + published post-commit; `cross_episode_decision` → `llm_verifier.rs` arm (covers candle 270m/27b + ollama + cloud_api via delegation); worker key fix → `apply_supersede` envelope_kind match. Proven WIRED by `memory_update_production_path` (real `ingest_structured_inner`) + `memory_update_supersede` (real `apply_supersede` at canonical keys).
- [x] DEAD-CODE (code) — no orphan symbols; candle/cloud verify files declared-but-untouched are intentionally covered by the shared `LlmVerifier` arm; `existing_object` `map_or(oid,..)` fallback is an unreachable-but-defensive default (loser_fact_id always sourced from `prior`).
- [x] SEMANTIC — read the adversarial review in full + re-verified BUG-1 reachability (re-assert with `valid_to`, then a disjoint third fact → false supersede under the stale window; fixed).

### GATE RECORD
Outcome: PASS
Flag (architecture residue, escalated per autonomy:auto verify-gate rule; owner delegated the decision in-session): the frozen §3/§6 predicted the verify worker needed "no change expected", but build discovered `worker::apply_supersede` minted KV keys via a local `format!("{kind}:{ulid}")` (scope-less) instead of the canonical `lunaris:{scope}:{kind}:{ulid}`. Because `read_as_of` matches the literal key and ingest writes the full key, the supersede LOSER was never found nor closed on every real backend (embedded/Moon/PG) — the contract's bi-temporal "After" guarantee (recall-as-of-now returns only the winner) silently did not hold. The pre-existing `verify_pipeline_smoke` test passed by accident (its mock keyed to the same buggy relative string). Fixed to mint via `lunaris_core::keyspace::{entity,relation,fact}_key` (CONVENTIONS.md-mandated); pinned by `memory_update_supersede::supersede_closes_loser_at_canonical_fact_key` (red→green) + the corrected `verify_pipeline_smoke` fixture. Shared-component change (all supersede reasons benefit) within the declared §5 scope.
Reviewed by: Tin Dang (delegated, auto-mode) · date: 2026-06-15

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): cross-episode-contradiction publish rate to `__lunaris_verify__`; supersede-apply success rate (loser bt closed); dedup ratio (re-asserted triples / total fact writes); false-supersede rate (should stay 0 — `no_false_supersede`).
Spec delta for the next loop: (1) full LLM-SEMANTIC arbitration of cross-episode contradictions is deferred — v1 uses deterministic latest-assertion-wins because `NeedsReviewReason::CrossEpisodeContradiction` carries fact IDs + object IDs but not the fact TEXTS; threading the texts to the verifier is the follow-up. (2) The spo-index read-modify-write is per-call `read_as_of(now)`, not transactional — high-concurrency same-`(subject,predicate)` ingests may miss a contradiction on one pass (caught next pass / by the verifier). (3) The supersede WINNER reset (`apply_supersede` sets winner `valid=(now,None)`) is a v0 simplification carried over — confirm it's correct for cross-episode (vs intra-episode) arbitration.

### Competency deltas
- [TDD · folded] A discriminating test for a storage-keyed operation MUST seed at the REAL production key, not a convenient relative string — `worker::apply_supersede` passed its unit test for releases while silently broken because the mock was keyed to the same buggy scope-less string (evidence: `memory_update_supersede` red→green exposed the `format!("fact:{ulid}")` key-mint that `verify_pipeline_smoke` masked).
- [TDD · folded] The adversarial subagent refute-read is load-bearing, not ceremonial: it caught a real MEDIUM correctness bug (stale spo-index window → false supersede on re-assert-with-narrowed-window) that the §4 plan and my own green both missed (evidence: BUG-1, fixed red→green via `test_reasserted_narrowed_window_keeps_spo_index_fresh`).
- [ADD · folded] When a frozen contract's "no change expected" for a dependency proves false at build AND the fix is convention-mandated + within declared scope, fixing in-scope (with a discriminating red→green + a scope/§6 flag) beats a change-request round-trip — the contract predicted, it did not constrain (evidence: worker key-mint fix, CONVENTIONS.md "local key-mint = bug").
- [SDD · folded] The §4 test plan correctly listed the discriminating integration tests (dedup/publish/INGEST-04/supersede) but the first build under-delivered them as unit-only — the spec bundle should make "production-path integration test per scenario" a hard exit-gate item, not an implicit expectation (evidence: integration suite added during build-completion, not tests phase).
