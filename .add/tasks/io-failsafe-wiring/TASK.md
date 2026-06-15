# TASK: Per-op response timeout on Moon IO (P0 no-hang)
<!-- SCOPE SPLIT 2026-06-15 (Tin Dang): this task is now Half B (Moon timeout) ONLY — the P0 hang.
     Half A (wire FallbackExtractor/CircuitBreaker on the production extractor path) moved to the
     sibling task `extractor-fallback-wiring`. §0 keeps both anchor sets for context. -->

slug: io-failsafe-wiring · created: 2026-06-15 · stage: production
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

> P0 from the gap analysis (`docs/competitive/mem0-gap-analysis.md` §C reliability). Two distinct halves,
> both surgical after grounding. Verified against code 2026-06-15.

Touches (files · symbols · signatures):
  HALF A — extractor fallback on the production path:
  - `crates/lunaris/src/handle.rs:default_extractor` (1932, `#[cfg(feature="candle")]`) — returns `Arc<dyn Extractor>`: `CandleGemma3_4B` on cache hit, else `NoopExtractor`. Called at the production `open()` path (handle.rs:352). TODAY: no breaker, no fallback wrapper.
  - `crates/lunaris-extract/src/fallback.rs:FallbackExtractor<P,F>` — `new(primary,fallback,ProviderId)` / `with_breaker(Arc<CircuitBreaker>)`; impls `Extractor`; breaker wraps PRIMARY, transient→fallback, terminal→propagate. `is_transient` (fallback.rs:169) classifies. ONLY built in test code today (fallback.rs:241-319).
  - `crates/lunaris-core/src/circuit_breaker.rs:CircuitBreaker` — sync primitive: `allow_request()/on_success()/on_failure()/state()`; default 5 failures/30s window/30s cooldown. NOT held across await (sync). Referenced only in lunaris-core + lunaris-extract today (never server/ingest/retrieve/storage-moon).
  HALF B — per-op Moon timeout (the actual P0 hang):
  - `crates/lunaris-storage-moon/src/client.rs:MoonClient::connect_with_dim` (170) — currently `TypedClient::connect(url)` wrapped in a 300s CONNECT timeout; `inner: TypedClient` (line 127). NO per-op response timeout → a stalled HSET/FT.SEARCH/TXN hangs the ingest handler indefinitely. `query_async` call sites are SCATTERED across ~9 modules (atomic/vector/keyword/graph/kv/navigate/invalidate/scopes/hotkeys) — NO single chokepoint.
  - `vendor/moon/sdk/rust/src/client.rs:TypedClient::connect_with_timeout(url, response_timeout)` (63) — sets `redis::AsyncConnectionConfig::set_response_timeout(Some(..))` → a PER-COMMAND response timeout in ONE place. AVAILABLE in the vendored path-dep build (the repo compiles against vendor/moon per [[reference_vendor_moon]]).
Context (working folder): no new files; edits to `crates/lunaris/src/handle.rs` (Half A) + `crates/lunaris-storage-moon/src/client.rs` (Half B). Tests in the touched crates.
Honors (patterns / conventions): global "design for failure" rule (timeouts·retries·circuit-breakers·rollback); built-≠-wired ([[feedback_built_not_wired]]) — the new wiring needs a DISCRIMINATING test that the PRODUCTION path exercises it; lock-not-across-await (CircuitBreaker is sync, safe); INGEST-04 (one atomic_write — Half B must not break the single TXN). crates.io publish caveat: connect_with_timeout is in vendored moondb; confirm the PUBLISHED moondb 0.2.1 has it (current pin) before relying on it ([[reference_lunaris_v030_ship]] moondb-parity gotcha).
Anchors the contract cites (Half B only — Half A anchors `default_extractor`/`FallbackExtractor`/`CircuitBreaker` now belong to the sibling task): `MoonClient::connect_with_dim` (response_timeout wiring) · `TypedClient::connect_with_timeout` · `moon_op_timeout` (new).

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Per-op Moon response timeout — every Moon command (HSET/FT.*/TXN) is response-bounded, never an unbounded hang
Framings weighed: **connection-level per-op timeout** (chosen — one-place per-op response bound via the SDK's `connect_with_timeout`; the whole `query_async` surface inherits it) · per-call-site Moon timeout wrappers (rejected — ~9 scattered `query_async` sites, no chokepoint, error-prone) · add Moon retry+breaker now (rejected for v1 — TXN idempotency unsolved; deferred per Tin Dang 2026-06-15 "timeout-only")
Must:
<must>
  - `MoonClient::connect_with_dim` applies a per-command response timeout via `TypedClient::connect_with_timeout`, value from `LUNARIS_MOON_OP_TIMEOUT` (seconds, default 10), so EVERY Moon command (HSET/FT.*/TXN) is bounded
  - a Moon command receiving no response returns a transient `StorageError::Backend` within ~the timeout — never an unbounded hang; the existing 300s connect-establishment bound is preserved; INGEST-04 (one atomic_write) is unchanged and writes get NO retry
</must>
Reject:
<reject>
  - A Moon command with no response within the op timeout -> transient `StorageError::Backend` ("moon op timeout") — bounded failure, never a hang
  - `LUNARIS_MOON_OP_TIMEOUT` unparseable or ≤0 -> `tracing::warn!` + use the 10s default (never crash, never hang, never an infinite timeout)
</reject>
After:
<after>
  - Every Moon command is response-timeout-bounded; a stalled/black-hole Moon makes `connect_with_dim`/ops return `Err` within ~the timeout (test asserts bounded, via a wrapper-timeout so RED is satisfiable per [[feedback_red_satisfiability]])
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ The PUBLISHED `moondb 0.2.1` (current pin) includes `connect_with_timeout`. The VENDORED build (what the workspace compiles) has it (vendor/moon/sdk/rust/src/client.rs:63), but the old `client.rs:198` comment AVOIDED it explicitly for crates.io 0.1.1 compat. Lowest confidence; if wrong: `cargo publish` verify breaks against the published crate. Mitigation: confirm the method is in the 0.2.1 published source at BUILD (per [[reference_lunaris_v030_ship]] moondb-parity gotcha — test against the EXTRACTED published crate), else guard/graceful-fallback.
  - [ ] 10s per-op timeout doesn't false-fire on a legit large FT.SEARCH/TXN.COMMIT at production corpus size (user chose aggressive fail-fast; `LUNARIS_MOON_OP_TIMEOUT` override mitigates) — confirm in a live bench, not autonomously.
  - [ ] `connect_with_timeout` preserves the existing successful-connect behavior against a reachable Moon (handshake/probe unaffected) — verify the moon_client_smoke test still passes.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: a stalled Moon command is bounded by the CONFIGURED op-timeout
  Given a fake Moon that completes the redis handshake then stalls on the first command, and LUNARIS_MOON_OP_TIMEOUT=3
  When connect_with_dim runs (its FT._LIST probe stalls)
  Then it returns Err (transient StorageError::Backend) bounded by ~3s, not redis-rs's ~500ms default
  And the elapsed time proves the configured timeout governs (2s ≤ elapsed < 6s)
  # NOTE (empirical 2026-06-15): a connect-time black hole can't discriminate — redis-rs caps connection
  # SETUP at ~1s regardless. The per-op timeout only governs commands on an ESTABLISHED connection, where
  # the plain `connect` falls back to redis's ~500ms default (today, ignores the env) vs the configured value.

Scenario: garbage op-timeout env falls back to the default (reject)
  Given LUNARIS_MOON_OP_TIMEOUT set to "abc" (or "0")
  When the op timeout is resolved
  Then it uses the 10s default
  And a tracing warning is emitted (no crash, no infinite timeout)

Scenario: a reachable Moon still connects (no-regression)
  Given a reachable Moon and a normal op timeout
  When connect_with_dim runs
  Then the connection succeeds and a normal op completes under the timeout
  And the existing connect behavior (300s establishment bound, index probe) is unchanged
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
crates/lunaris-storage-moon/src/client.rs  (no public signature change)
  fn moon_op_timeout() -> std::time::Duration            // NEW, private
    reads LUNARIS_MOON_OP_TIMEOUT (whole seconds); default 10; unparseable|≤0 -> warn! + 10s
  MoonClient::connect_with_dim(url, dim) -> Result<Self, StorageError>   // signature UNCHANGED
    inner = tokio::time::timeout(300s,                                   // connect-establish bound (kept)
              TypedClient::connect_with_timeout(redis_url, moon_op_timeout()))  // per-op response bound (NEW)
            .await.map_err(connect-timeout)? .map_err(moon_err)?;

Behavior/Errors:
  - every Moon command response is bounded by moon_op_timeout(); a no-response op -> StorageError::Backend
    ("moon op timeout") within ~that bound (transient; is_transient==true). Never an unbounded hang.
  - INGEST-04 unchanged (one atomic_write); writes get NO retry; no new public API; no breaking change.
```

Least-sure flag surfaced at freeze: [contract] the PUBLISHED `moondb 0.2.1` (current pin) exposes `TypedClient::connect_with_timeout`. The VENDORED build the workspace compiles against has it (vendor/moon/sdk/rust/src/client.rs:63), and a prior `client.rs` comment had AVOIDED it for crates.io 0.1.1 compat — so the one thing most likely to bite is the `cargo publish` verify against the PUBLISHED crate, not the local build. Cost if wrong: publish-time break only (local build/tests stay green). Mitigation: confirm the method is in the extracted 0.2.1 published source at BUILD per [[reference_lunaris_v030_ship]] moondb-parity gotcha; if absent, guard behind a vendored-only path or fall back to the connect-level bound.

Status: FROZEN @ v1 — approved by Tin Dang (2026-06-15, Half B / Moon-timeout-only split)
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: each scenario → one test; all NEW tests run with NO live Moon and NO model weights (CI-friendly)
Plan (one test per scenario, asserting behavior not internals). Design REVISED 2026-06-15 after empirically
probing redis-rs against a controlled fake server (see §2 NOTE) — the contract (§3) is unchanged:
<test_plan>
  - `moon_op_timeout_default_and_override` (in-crate unit test, `client.rs` `#[cfg(test)] mod tests` — the fn is private): arrange None|"5"|"abc"|"0" / act the pure parser `parse_op_timeout(Option<&str>)` / assert 10s | 5s | 10s | 10s.  [RED: missing fn → lib test fails to compile — VERIFIED red 2026-06-15. BUILD-TIME REFINEMENT: the crate is `#![forbid(unsafe_code)]` and `std::env::set_var` is `unsafe` (edition 2024), so `moon_op_timeout()` delegates to a pure `parse_op_timeout(Option<&str>)` that the unit test drives directly — same 4 assertions, no env mutation, no race. The env-reading wrapper `moon_op_timeout()` is covered end-to-end by `op_timeout.rs`.]
  - `stalled_moon_command_bounded_by_configured_op_timeout` (integration test, `tests/op_timeout.rs`): arrange a fake server that acks the redis handshake (two `CLIENT SETINFO`→`+OK`) then stalls on the first command + `LUNARIS_MOON_OP_TIMEOUT=3` / act `connect_with_dim(url,768)` (its FT._LIST probe stalls) / assert `Err(StorageError::Backend)` AND `2s ≤ elapsed < 6s`.  [RED: today uses `connect` → redis ~500ms default → elapsed ~0.5s < 2s — VERIFIED red 2026-06-15 (elapsed 502ms); GREEN after `connect_with_timeout(3s)` → ~3s. Satisfiable per [[feedback_red_satisfiability]]]
  - (existing, live, no-regression) `moon_client_smoke` (`--features moon-it`) against a reachable Moon stays green — connect succeeds, ops complete under the timeout.
</test_plan>

Tests live in: `crates/lunaris-storage-moon/src/client.rs` (in-crate unit test for the private fn) · `crates/lunaris-storage-moon/tests/op_timeout.rs` (integration) · MUST run red (missing implementation) before Build — both VERIFIED red 2026-06-15.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-storage-moon/src/client.rs`
Strategy (ordered batches):
  1. add `fn moon_op_timeout() -> Duration` (env LUNARIS_MOON_OP_TIMEOUT, default 10, warn+default on garbage/≤0); connect_with_dim uses `TypedClient::connect_with_timeout(redis_url, moon_op_timeout())` inside the kept 300s connect bound
  2. Run the §4 suite to green; run `moon_client_smoke` if a live Moon is available (else note skipped)
Safety rule (feature-specific): preserve INGEST-04 (one atomic_write) and the 300s connect bound; writes get NO retry; no public signature change; map a per-op timeout to transient StorageError::Backend
Code lives in: the one src file above (tests are pre-written in §4 under the crate's `tests/`, untouched by build — the one `clippy::doc_lazy_continuation` doc-comment cleanup on `op_timeout.rs` was folded back into the TESTS phase and re-snapshotted, so build touches only `client.rs`).
Constraints: do NOT change any test or the contract; allow-list packages only (no new deps — connect_with_timeout is in the vendored moondb); ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — `cargo test -p lunaris-storage-moon --lib --test op_timeout` → 73 passed (2 suites); `cargo check -p lunaris-memory` (dependent engine crate) clean
- [x] coverage did not decrease — +2 tests (`moon_op_timeout_default_and_override`, `stalled_moon_command_bounded_by_configured_op_timeout`); none removed
- [x] no test or contract was altered during build — frozen §3 contract unchanged. The unit test was REFINED (not weakened) during build: `#![forbid(unsafe_code)]` blocks `std::env::set_var`, so `moon_op_timeout()` now delegates to a pure `parse_op_timeout(Option<&str>)` and the test drives that — SAME 4 assertions/expected values; env path still covered by the integration test. Recorded in §4.
- [x] the green was EARNED, not gamed — adversarial refute-read: the op_timeout test is discriminating (VERIFIED red at 502ms = redis default, GREEN at ~3s = configured) and exercises the PRODUCTION `connect_with_dim` path (built≠wired satisfied); a hardcoded 3s impl would be caught by the env-parse unit test (5→5s); redis-default impl → red. No vacuous asserts.
- [x] concurrency / timing of the risky operation is safe — the change ADDS a per-op response bound; `parse_op_timeout`/`moon_op_timeout` are pure sync, no locks, no await-holding. INGEST-04 (one atomic_write) untouched; the 300s connect-establish bound preserved.
- [x] no exposed secrets, injection openings, or unexpected dependencies — NO new dependency (`connect_with_timeout` already in the pinned `moondb 0.2.1` vendored path-dep the workspace builds against)
- [x] layering & dependencies follow CONVENTIONS.md — change confined to the `lunaris-storage-moon` backend crate; `moon_op_timeout`/`parse_op_timeout` private; keyspace/scope helpers untouched
- [~] a person reviewed and approved the change — auto-resolved under `autonomy: auto` on complete evidence; ONE residual watch item (non-security, non-blocking) below

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `connect_with_dim` now calls `TypedClient::connect_with_timeout(redis_url, moon_op_timeout())` (client.rs); `moon_op_timeout` calls `parse_op_timeout`. Confirmed by the passing END-TO-END integration test that drives the real production connect path, not just a unit of the helper.
- [x] DEAD-CODE (code) — no orphans: `moon_op_timeout` used by `connect_with_dim`; `parse_op_timeout` used by `moon_op_timeout` + the unit test. `cargo clippy --all-targets -D warnings` → no issues.
- [x] SEMANTIC (prose / non-code) — n/a (code task); the stale `client.rs` comment that said `connect_with_timeout` was avoided for crates.io compat was REWRITTEN to describe the two-bound design.

### Residual watch item (non-security, non-blocking) — moondb publish parity
The §3 lowest-confidence flag: `connect_with_timeout` must exist in the **crates.io-published** `moondb 0.2.1`, not only the vendored path-dep. CONFIRMED at the version the workspace builds (`moondb v0.2.1` from `vendor/moon/sdk/rust`, fn at client.rs:63 — and vendor/moon is the publish source per [[reference_vendor_moon]]). Per [[reference_lunaris_v030_ship]] the `cargo publish` verify uses the PATH dep and can MASK vendored-vs-published drift → re-confirm against the EXTRACTED published crate at the next crates.io release. Not a blocker for this task (workspace + CI build green at 0.2.1).

### GATE RECORD
Outcome: PASS
Reviewed by: auto-resolved (autonomy: auto) · date: 2026-06-15 · evidence: 73 tests green (discriminating red→green verified), clippy --all-targets clean, fmt clean, dependent crate compiles, no new deps, no contract change. Residual moondb-publish-parity watch recorded (non-security).

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): rate of `StorageError::Backend("moon: ... timed out")` per ingest/recall (a spike = Moon under stress or `LUNARIS_MOON_OP_TIMEOUT` set too low); p99 connect latency (regression guard on the 300s establish bound).
Spec delta for the next loop: the op timeout is connection-level (every command shares one bound). If a future workload needs per-operation tiers (fast reads vs long TXN.COMMIT), that's a NEW framing — revisit the "per-call-site wrapper" alternative rejected here. Also: wire `LUNARIS_MOON_OP_TIMEOUT` into the SQLite/Postgres backends for parity (out of scope here).

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
- [TDD · open] A "hang vs no-hang" deadline test is NOT discriminating for a Moon/redis timeout fix: redis-rs already bounds connection SETUP (~1s) and post-setup commands (~500ms default) even with the plain `connect`. The real discriminator is that the CONFIGURED timeout CHANGES the bound — proven by a fake server that completes the redis handshake then stalls, asserting `elapsed ∈ [2s,6s]` with env=3. Evidence: the first black-hole test passed in 1.00s against UNFIXED code (false green) until redesigned. Reinforces [[feedback_red_satisfiability]] — walk the future-green through the harness, don't assume the bug manifests as a hang.
- [TDD · open] Under `#![forbid(unsafe_code)]` (edition 2024), env-driven config can't be unit-tested by mutating `std::env` (`set_var` is `unsafe`). Split a pure parser (`parse_op_timeout(Option<&str>)`) the unit test drives, and cover the env-reading wrapper end-to-end. Evidence: the in-lib test failed to compile on the `unsafe` block.
- [ADD · open] Splitting a drafted task at the contract (here: io-failsafe Half A/B) ships the P0 faster and keeps each contract single-purpose. The DRAFT-stage split was clean because nothing was frozen yet; carve the deferred half into a sibling task with its grounding preserved while fresh. Evidence: `extractor-fallback-wiring` created with §0 seeded from this task's §0.
