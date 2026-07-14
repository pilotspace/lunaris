# TASK: Moon backend parity honesty (dedupe, as_of, scratchpad key read)

slug: moon-parity-honesty · created: 2026-07-14 · stage: production
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

Touches (files · symbols · signatures): `crates/lunaris-core/src/keyspace.rs` (new `dedupe_key(scope, raw)` — blake3-hashed raw, `lunaris:{scope}:dedupe:{hex}`; blake3 already a core dep); `crates/lunaris-storage-moon/src/lib.rs:213 impl StoragePort` (override trait-default `lookup_by_dedupe_key`/`insert_dedupe_key` — typed client `get`/`set_nx` at vendor sdk client.rs:167/221); `crates/lunaris/src/handle.rs:ingest_idempotent` (already routes through the trait — no change); `crates/lunaris/src/primitives/working_memory.rs:find` (fallback classifier `is_keyword_not_supported` only catches NotSupported — Moon `Backend("… empty query after analysis")` and `Backend("… unknown index")` propagate as errors); `crates/lunaris-mcp/src/tools/recall.rs` (as_of parsed at :143, silently ignored downstream on Moon — `state.lunaris.storage().capabilities().bi_temporal_native` is false on Moon, true on embedded).
Context (working folder): findings memory `project_lunaris_mcp_deep_test_findings` §2 (3 dupes minted live), §3 (as_of=2020 returned 2026 rows), §4 (key `state` write-OK/read-impossible; fresh scope `unknown index`).
Honors (patterns / conventions): keyspace helpers only in lunaris-core (RC-1); trait-default fall-through documented as v0.5 boundary (handle.rs:1156) — this task closes it for Moon; MCP DTO deny_unknown_fields untouched.
Anchors the contract cites: `keyspace::dedupe_key`, `MoonStorage::{lookup_by_dedupe_key,insert_dedupe_key}`, `WorkingMemory::find` fallback, `recall.rs` as_of guard, `IngestKind::Duplicate`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: the Moon backend keeps the promises the MCP contract already makes — dedupe dedupes, as_of refuses instead of lying, exact-key scratchpad reads never depend on FT text analysis of the key.
Framings weighed: implement-on-Moon + refuse-loudly-where-unimplemented (chosen) · implement full bi-temporal recall on Moon (design-spike scope, defer) · doc-only caveats (leaves silent wrongness).
Must:
<must>
  - Moon lookup_by_dedupe_key/insert_dedupe_key: KV sidecar at keyspace::dedupe_key(scope, raw); insert is first-writer-wins (SET NX); lookup returns the prior Lsn
  - ingest_idempotent on Moon: same (scope, dedupe_key) twice -> second returns IngestKind::Duplicate(prior_lsn), no second episode
  - memory.recall with as_of on a backend where capabilities().bi_temporal_native == false -> typed InvalidInput naming the backend gap (never silently-ignored)
  - as_of on embedded (bi_temporal_native == true) keeps working exactly as today
  - WorkingMemory::find: Moon FT 'empty query after analysis' on the fused root -> vector-only fallback (same path as keyword-NotSupported); 'unknown index' (fresh scope, nothing ingested) -> Ok(empty) so read() returns found:false
</must>
Reject:
<reject>
  - as_of + Moon -> "as_of requires a bi-temporal backend" (typed InvalidInput, -32602)
</reject>
After:
<after>
  - scratchpad key "state" written on Moon reads back verbatim; a brand-new scope's first read returns found:false, not an error
  - the MCP dedupe/as_of tool descriptions are true on the shipped backends
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ String-matching Moon FT error text ("empty query after analysis" / "unknown index") is stable — lowest confidence because it couples to Moon's error wording; if wrong: fallback misses and the old error returns (no silent wrongness, just the pre-fix behaviour); pinned by live tests so drift is caught.
  - [x] typed set_nx/get exist on the vendored SDK — confirmed client.rs:167/221.
  - [x] embedded keeps bi_temporal_native=true so the as_of gate cannot regress SQLite (embedded lib.rs:563).
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: dedupe key is idempotent on Moon (live)
  Given an episode ingested via ingest_idempotent with dedupe key K under scope S on live Moon
  When ingest_idempotent runs again with the SAME key K and payload
  Then the second call returns IngestKind::Duplicate carrying the first call's Lsn
  And recall surfaces exactly one episode for the content

Scenario: dedupe keys are scope-isolated (live)
  Given key K used under scope S1
  When ingest_idempotent uses the same K under scope S2
  Then the S2 call is Fresh (not a duplicate)

Scenario: stopword key reads back on Moon (live)
  Given WorkingMemory::write("state", value) under a fresh scope on live Moon
  When WorkingMemory::read("state") runs
  Then it returns Some(value) verbatim
  And no error is surfaced

Scenario: fresh-scope read returns none (live)
  Given a brand-new scope with zero ingested rows
  When WorkingMemory::read("anything") runs
  Then it returns Ok(None)
  And no "unknown index" error escapes

Scenario: as_of on Moon is refused loudly
  Given a storage whose capabilities report bi_temporal_native == false
  When memory.recall is called with as_of set
  Then the call errors InvalidInput naming the bi-temporal gap
  And no results are returned

Scenario: as_of on embedded still works
  Given the embedded backend (bi_temporal_native == true)
  When memory.recall is called with as_of set
  Then the existing Wave A.1 snapshot behaviour is unchanged
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
lunaris_core::keyspace::dedupe_key(scope: &Scope, raw: &str) -> Vec<u8>
  = "lunaris:{scope}:dedupe:{blake3_hex(raw)}"   (raw is arbitrary caller data — hashed, never spliced)
MoonStorage::insert_dedupe_key(scope, raw, lsn)  -> SET NX at dedupe_key; first writer wins; value = JSON Lsn
MoonStorage::lookup_by_dedupe_key(scope, raw)    -> GET; Some(Lsn) | None
WorkingMemory::find fused-root error handling:
  keyword NotSupported          -> vector-only retry      (unchanged)
  Backend msg ~ "empty query after analysis" -> vector-only retry (NEW)
  Backend msg ~ "unknown index"              -> Ok(vec![])        (NEW — fresh scope)
  vector-only retry erroring "unknown index" -> Ok(vec![])        (NEW)
memory.recall handler: params.as_of.is_some() && !storage.capabilities().bi_temporal_native
  -> ToolError::InvalidInput("as_of requires a bi-temporal backend; <backend> reads current state only")
MCP tool descriptions: dedupe_key + as_of caveats updated to match (Postgres keeps trait-default fall-through)
```

Least-sure flag surfaced at freeze: [contract] error-text matching for the FT fallback (see §1 ⚠) —
accepted with live pins. [spec] SET NX sidecar has no TTL — dedupe keys accrete per scope (one small
string per ingest with a key); acceptable now, TTL/eviction recorded as a §7 delta.

Status: FROZEN @ v1 — approved by Tin Dang (delegated fully-auto, standing "keep going" 2026-07-14)

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every scenario; live suite gated LUNARIS_TEST_MOON_URL; as_of gate unit-level (Moon-shaped caps) + embedded regression via existing Wave A.1 test.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - dedupe_key_idempotent_on_moon (live): two ingest_idempotent, second is Duplicate(prior), episode count 1
  - dedupe_key_scope_isolated_on_moon (live): same key, two scopes, both Fresh
  - scratchpad_stopword_key_reads_back_moon (live): write/read "state" round-trips
  - scratchpad_read_fresh_scope_returns_none_moon (live): brand-new scope read -> Ok(None)
  - as_of_rejected_on_non_bitemporal_backend (unit): guard errors with Moon-shaped caps, passes with embedded-shaped caps
  - as_of on embedded: existing recall.rs Wave A.1 test stays green (regression witness)
</test_plan>

Tests live in: `crates/lunaris/tests/moon_parity.rs` · guard unit in `crates/lunaris-mcp/src/tools/recall.rs` tests mod · MUST run red (missing implementation) before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-core/src/keyspace.rs` `crates/lunaris-storage-moon/src/lib.rs` `crates/lunaris-storage-moon/src/kv.rs` `crates/lunaris/src/primitives/working_memory.rs` `crates/lunaris/src/handle.rs` `crates/lunaris/tests/moon_parity.rs` `crates/lunaris-mcp/src/tools/recall.rs` `crates/lunaris-mcp/src/main.rs` `crates/lunaris-retrieve/src/hydrate.rs`
Strategy (ordered batches): 1. keyspace::dedupe_key (+unit) 2. Moon sidecar overrides 3. find() fallback arms 4. as_of guard + descriptions 5. live suite green + workspace lints.
Safety rule (feature-specific): SET NX for insert (never clobber a prior LSN); error-text matching scoped to StorageError::Backend only; as_of gate must not touch the embedded path.
Code lives in: `crates/`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — moon_parity 4/4 live (Moon 6399), forget_scoped_moon 5/5 regression, lunaris-mcp 7 suites all ok (incl. new as_of gate units + Wave A.1 embedded as_of witness), hydrate suites 4/4 + 3/3
- [x] coverage did not decrease — 6 new tests (4 live + 2 unit), none removed
- [x] no test or contract was altered during build
- [x] the green was EARNED — dedupe red (3 Fresh writes), stopword-key red (empty query error), fresh-scope red (unknown index error); all observed pre-fix on live Moon
- [x] concurrency / timing — SET NX first-writer-wins closes the replay race at the store; no locks held across await
- [x] no exposed secrets / injection — dedupe raw keys are blake3-hashed before key-minting (cannot alias the keyspace); error-text matching confined to StorageError::Backend
- [x] layering — keyspace helper in lunaris-core (RC-1); sidecar in lunaris-storage-moon; fallback in the WorkingMemory primitive; gate in the MCP handler
- [x] reviewed — self-review (delegated fully-auto)

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
- [x] two identical dedupe-keyed ingests on Moon = ONE episode + Duplicate(prior LSN) — confirmed live
- [x] scratchpad "state" key round-trips on Moon; fresh scope reads found:false — confirmed live
- [x] as_of + Moon-shaped caps = typed InvalidInput naming the gap; embedded path untouched — confirmed by unit pair + existing Wave A.1 test staying green

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — dedupe_key used by kv::lookup_dedupe/insert_dedupe, wired via MoonStorage overrides, exercised by ingest_idempotent live; is_ft_query_unusable/is_ft_index_missing called in find(); ensure_as_of_supported called in recall handler
- [x] DEAD-CODE (code) — no orphans; trait defaults remain for Postgres (documented)
- [x] SEMANTIC (descriptions) — recall/record_decision/record_edit tool descriptions re-read; caveats now match runtime behaviour

### GATE RECORD
Outcome: PASS
Reviewed by: Claude (delegated fully-auto by Tin Dang) · date: 2026-07-14

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): was_duplicate rate on hook ingests (should be >0 under replays); InvalidInput rate on as_of.

### Spec delta
- [SPEC · open] dedupe sidecar has no TTL — one small key accretes per dedupe-keyed ingest; add TTL/eviction policy (evidence: §3 freeze flag)
- [SPEC · open] Moon AS_OF parity (STORE-07) — implement bi-temporal reads on Moon, then drop the recall gate (evidence: gate exists only because reads ignore as_of)
- [SPEC · open] FT error-text matching ("empty query after analysis"/"unknown index") should become typed Moon error codes in the SDK (evidence: §1 ⚠ wording coupling)
- [SPEC · open] Moon-only direction (Tin, 2026-07-14): PG/SQLite deprecate-then-delete makes the Postgres dedupe fall-through moot — fold into the moon-only milestone (evidence: user directive this session)

### Competency deltas
- [TDD · open] "refuse loudly where unimplemented" is testable with capability-shaped unit fixtures — no live backend needed for honesty gates (evidence: as_of gate unit pair)
- [DDD · open] idempotency belongs at the storage port, not the caller — the trait-default fall-through hid a per-backend behavioural fork for two versions (evidence: dedupe live repro)
