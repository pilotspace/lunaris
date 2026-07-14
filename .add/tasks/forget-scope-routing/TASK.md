# TASK: Scope-aware forget pipeline (Wave 1D)

slug: forget-scope-routing · created: 2026-07-14 · stage: production
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

Touches (files · symbols · signatures): `crates/lunaris/src/forget.rs:scan_matches` (hard-coded `Scope::dev()` + bare `episode:` prefix — the silent no-op); `crates/lunaris/src/forget.rs:build_soft_delete_op` (MVCC sys_to JSON-patch, reusable as-is); `crates/lunaris/src/handle.rs:1256:ScopedLunaris::forget` (deprecated-delegating shim to replace); `crates/lunaris-core/src/keyspace.rs:episode_key/scope_prefix` (`lunaris:{scope}:episode:{ulid}`); `crates/lunaris-storage-moon/src/kv.rs:scan_range` (ignores scope arg — caller passes scoped prefix; buffers full result); `crates/lunaris-retrieve/src/hydrate.rs:hydrate/hydrate_mixed` (ZERO sys_to handling — soft-deleted rows still hydrate); `crates/lunaris-mcp/src/tools/forget.rs:handle` (already calls scoped.forget — unchanged).
Context (working folder): findings ledger in memory `project_lunaris_mcp_deep_test_findings` §1; live repro: forget by prefix AND by exact ULID returned removed:0 on Moon 6381 scope git_487b86f2f5774fbd; docs/v0.3-known-debt.md names this Wave-1D debt.
Honors (patterns / conventions): keyspace helpers ONLY from `lunaris_core::keyspace` (RC-1); one `atomic_write` per forget call (D-19); `Scope::dev()` is a migration crutch — new call sites forbidden; never hold a lock across .await; thiserror-typed errors.
Anchors the contract cites: `ScopedLunaris::forget`, `scan_matches_scoped` (new), `keyspace::scope_prefix`, `hydrate` sys-gate, `ForgetReceipt.rows_written`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: scope-aware forget (Wave 1D) — deletion that actually deletes under the bound scope, visible to recall.
Framings weighed: scoped soft-delete + read-path sys-gate (chosen) · hard KvDelete + index purge (bigger blast radius, loses audit/MVCC) · reject-forget-on-Moon-until-v1 (honest but ships nothing).
Must:
<must>
  - ScopedLunaris::forget(Id(ulid)) reads `lunaris:{scope}:episode:{ulid}` under self.scope and soft-deletes it (bt.sys.1 = now) via ONE atomic_write under self.scope
  - ScopedLunaris::forget(Scope(BySource(prefix))) scans `lunaris:{scope}:episode:` under self.scope and soft-deletes every match whose payload source starts with prefix
  - ForgetReceipt.rows_written equals the number of episodes actually stamped (>0 when matches exist)
  - hydrate/hydrate_mixed drop any hit whose resolved row (chunk's parent episode, or fact row) has bt.sys.1 = Some(_) — post-forget recall MISSES the episode content
  - dry_run still writes nothing and previews the true (scoped) match count
</must>
Reject:
<reject>
  - hard delete without confirmation token -> "ConfirmationRequired" (unchanged)
  - forget under scope A must never see or stamp scope B's rows -> cross-scope isolation (0 matches)
</reject>
After:
<after>
  - MCP memory.forget on live Moon returns removed>0 for an existing target and the episode no longer surfaces in memory.recall
  - deprecated Lunaris::forget (dev-scope) behavior unchanged
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Gating chunk hits on the PARENT episode row's sys_to is sufficient for "recall misses" — lowest confidence because chunk rows themselves stay sys-open and the FT index entry remains; if wrong: forgotten content still surfaces via a path that never consults the episode row (cost: exit criterion fails, needs chunk-row stamping too — v1 scope widen).
  - [x] Moon scan_range with a `lunaris:{scope}:episode:` prefix returns exactly this scope's episode rows — confirmed: kv.rs builds MATCH `<prefix>*` literally.
  - [x] build_soft_delete_op works unchanged on scoped keys — confirmed: it patches payload JSON + reuses m.key verbatim.
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: forget by episode id removes it from recall (live Moon)
  Given an episode ingested under scope S on live Moon whose content recalls at rank 1
  When ScopedLunaris(S).forget(Id(ulid)) runs
  Then the receipt has rows_written == 1
  And a subsequent scoped recall for the same query does NOT return that episode_id

Scenario: forget by source prefix removes all matches
  Given three episodes under scope S with source "wipe-me/a|b|c" and one with source "keep/x"
  When forget(Scope(BySource("wipe-me/"))) runs
  Then rows_written == 3
  And recall still returns the "keep/x" episode

Scenario: cross-scope isolation
  Given an episode under scope B with source "wipe-me/z"
  When ScopedLunaris(A).forget(Scope(BySource("wipe-me/"))) runs
  Then rows_written == 0
  And scope B's episode still recalls unchanged

Scenario: dry run previews without writing
  Given one matching episode under scope S
  When forget with dry_run runs
  Then preview == true and rows_written == 0
  And the episode still recalls unchanged

Scenario: hard delete without token rejected
  Given any target
  When forget(target.hard()) without confirmation_token runs
  Then it errors "ConfirmationRequired"
  And no row is touched

Scenario: sys-closed row dropped at hydrate (unit)
  Given a RawHit whose chunk's parent episode row has bt.sys.1 = Some(t)
  When hydrate runs
  Then the hit is absent from the output
  And sys-open siblings hydrate unchanged
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
ScopedLunaris::forget(request: Into<ForgetRequest>) -> Result<ForgetReceipt, LunarisError>
  soft (default): scan/read under self.scope via keyspace::scope_prefix(self.scope) + "episode:";
                  ONE atomic_write(&self.scope, sys_to-stamped KvPut ops); receipt.rows_written = ops.len()
  dry_run:        zero writes; receipt.preview = true
  hard w/o token: Err(Validate(ConfirmationRequired))   (unchanged)
  Id(ulid) fast path: read_as_of(&self.scope, keyspace::episode_key(scope, ulid))
Read-path gate: lunaris_retrieve::hydrate + hydrate_mixed drop rows where row.bt.sys.1.is_some()
  (chunk hits: gate on the parent EPISODE row fetched in the existing episode pass;
   fact hits in hydrate_mixed: gate on the fact row's own bt)
Schema: no new tables/keys; existing episode KV rows' bt.sys tuple; audit event unchanged
MCP wire: memory.forget unchanged ({removed: rows_written+rows_deleted})
Deprecated Lunaris::forget: body untouched (dev-scope, warns) — no behavior change
```

Least-sure flag surfaced at freeze: [spec] the ⚠ §1 assumption — episode-row sys-gating may not
hide content reachable without consulting the episode row (e.g. WorkingMemory::recover_value reads
the episode row directly — that one DOES consult it, fine; but a future retriever reading chunk rows
only would leak). Cost if wrong: live discriminator stays red → widen to chunk-row stamping in-task.
[contract] second flag: gating on `sys.1.is_some()` treats ANY sys-closed row as deleted for current
reads — correct today because supersede/invalidate semantics also mean "not current", but it couples
forget-visibility to MVCC invalidation semantics.

Status: FROZEN @ v1 — approved by Tin Dang (delegated fully-auto, standing "keep going" 2026-07-14)

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every scenario above; live-Moon suite env-gated (LUNARIS_HOOK_TEST_MOON_URL pattern).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - forget_id_removes_from_recall_moon (live, gated): ingest -> recall hits -> forget(Id) -> rows_written==1 -> recall misses
  - forget_prefix_removes_all_matches_moon (live, gated): 3x wipe-me/* + keep/x -> forget(BySource) -> rows_written==3 -> keep/x still recalls
  - forget_cross_scope_isolated_moon (live, gated): scope A forget never touches scope B
  - forget_dry_run_previews_without_writing_moon (live, gated)
  - forget_hard_without_token_rejected (unit, embedded)
  - hydrate_drops_sys_closed_rows (unit, mock port): sys-closed parent episode -> chunk hit dropped; sys-open sibling kept; hydrate_mixed fact-row variant
</test_plan>

Tests live in: `crates/lunaris/tests/forget_scoped_moon.rs` · `crates/lunaris-retrieve/tests/hydrate_sys_gate.rs` · MUST run red (missing implementation) before Build.

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris/src/forget.rs` `crates/lunaris/src/handle.rs` `crates/lunaris/tests/forget_scoped_moon.rs` `crates/lunaris-retrieve/src/hydrate.rs` `crates/lunaris-retrieve/tests/hydrate_sys_gate.rs`
Strategy (ordered batches): 1. hydrate sys-gate (+unit tests green) 2. scoped scan_matches + ScopedLunaris::forget real body (+unit) 3. live-Moon discriminators green 4. workspace clippy/fmt.
Safety rule (feature-specific): exactly ONE atomic_write per forget call (D-19); all storage calls under self.scope; deprecated dev-scope path byte-identical.
Code lives in: `crates/`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — hydrate_sys_gate 3/3 + hydrate_mixed 3/3 (regression) + forget_scoped_moon 5/5 on live Moon 6399 + lunaris-mcp forget units 6/6 + `cargo test --workspace` (sole failure = pre-existing hybrid_filter 6380-is-ai-proxy env gap; same suite passes 4/4 against real Moon 6399)
- [x] coverage did not decrease — 8 new tests, none removed
- [x] no test or contract was altered during build — red files byte-identical modulo rustfmt
- [x] the green was EARNED — discriminators observed RED first (rows_written 0 vs 1/3) on the exact live-bug repro; sys-gate red showed both hits hydrating
- [x] concurrency / timing — no locks; scan buffers then stamps; single atomic_write preserved (D-19); HLC ticks per row
- [x] no exposed secrets / injection — no new inputs; keys minted only via keyspace helpers (RC-1)
- [x] layering — forget pipeline stays in lunaris crate; hydrate gate in lunaris-retrieve; no new deps
- [x] reviewed — self-review + live end-to-end proof (delegated fully-auto)

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
- [x] MCP memory.forget on PRODUCTION Moon 6381 scope git_487b86f2f5774fbd returns removed>0 for the deep-test residue — confirmed: removed 1(deep-test/) + 3(decision:) + 1(edit:) + 4(scratchpad residue) via raw stdio JSON-RPC against the rebuilt binary
- [x] the forgotten sentinel no longer recalls — confirmed: recall "magic verification number deep test sentinel" returns 0 sentinel hits post-forget (only unrelated scratchpad row until it too was wiped)
- [x] cross-scope + dry-run + hard-token rails intact — confirmed: forget_scoped_moon invariant pins green pre- AND post-fix

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — forget_scoped referenced by ScopedLunaris::forget (handle.rs); scan_matches_scoped by forget_scoped; sys_closed by hydrate + hydrate_mixed (4 call sites); MCP handler unchanged and exercised live
- [x] DEAD-CODE (code) — no orphan symbols; deprecated Lunaris::forget untouched (still the dev-scope path, still warns)
- [x] SEMANTIC — n/a (code task)

### GATE RECORD
Outcome: PASS
Reviewed by: Claude (delegated fully-auto by Tin Dang) · date: 2026-07-14

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): forget receipts with rows_written==0 on non-empty scopes (regression signal); recall hit-rate on scopes with heavy forget traffic.

### Spec delta
- [SPEC · open] forget v1: stamp chunk:/fact: rows too — episode-row gating covers today's read paths, but a future retriever reading chunk rows directly would leak forgotten content (evidence: §1 ⚠ assumption; hydrate gates episode + chunk + fact rows now, FT index entries remain)
- [SPEC · open] hard-delete path via MCP (two-step token) — wire not exposed; soft-only today (evidence: forget.rs behaviour matrix)
- [SPEC · open] hybrid_filter test gate probes TCP only — 6380 ai-proxy Redis passes the probe and fails FT (evidence: workspace run; same foot-gun as installer default, fold into moon-parity-honesty or turnkey task)

### Competency deltas
- [TDD · open] invariant pins that PASS pre-fix (cross-scope, dry-run, hard-token) belong in the red suite anyway — they catch regressions the discriminators can't (evidence: 3 green pins + 2 red discriminators = right shape)
- [ADD · open] deep-test-first grounding (live repro before specify) made §0-§3 near-mechanical (evidence: this task, one pass, no re-work)
