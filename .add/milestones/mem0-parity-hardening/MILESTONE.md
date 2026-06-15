# MILESTONE: Mem0-Parity Production Hardening

goal: Lunaris is demonstrably production-ready and competitive with Mem0 — proven by a verified gap analysis, then by closing every P0 gap it surfaces (fail-safe IO, eval gates reproducing Mem0-comparable accuracy, and the prioritized capability/DX gaps).
rationale: intake bucket `new-major` — confirmed by Tin Dang 2026-06-14 ("Confirm & create as drafted" + "Run analysis in parallel now"). "Competitive production-hardening vs Mem0" is a new product theme no active milestone's goal covers: `bindings-gate-hardening` is narrow CI/bindings work, the pending Moon-only sweep is orthogonal. The request spans all four harden dimensions (reliability · eval · observability · correctness/security) AND all four Mem0 parity areas (memory-update intelligence · multi-level/categories · graph quality · SDK/DX) — too broad for a task or sub-milestone. Grounding (2026-06-14) showed the eval gauntlet (`lunaris-bench/src/eval/{locomo,longmemeval,er_f1}.rs`) and the circuit breaker (`lunaris-core/src/circuit_breaker.rs`) already EXIST → this is a coverage/wiring/competitiveness problem, not greenfield → gap-analysis-first de-risks before any build.
stage: production · status: active · created: 2026-06-14

> SDD living doc for this milestone. Keep it THIN: breadth, shared decisions, and
> exit criteria only — per-task detail lives in each `.add/tasks/<slug>/TASK.md`,
> written just-in-time. Update this doc whenever a task reveals a milestone gap.

## Scope
In:  competitive gap analysis (fresh current-Mem0 research vs Lunaris reality, claims verified against code) · the P0 hardening waves it ranks, drawn from {IO fail-safe *wiring* of the existing circuit breaker/retry across every external IO path · eval-gauntlet *CI gating* with Mem0-comparable numbers · LLM-driven memory-update intelligence (ADD/UPDATE/DELETE/NOOP) · multi-level memory + categories + metadata filtering · graph extraction quality/ergonomics · observability + progressive-rollout maturity · correctness/security hardening · SDK/integration DX}
Out: building a NEW eval harness or a NEW circuit breaker (both exist — we audit/wire/benchmark them, never re-implement) · live-Postgres parity (deferred to HUMAN-UAT per standing decision) · the Moon-only backend deprecation sweep (its own pending milestone) · multi-shard `TXN BEGIN PIN` (upstream Moon) · any parity claim drafted from stale memory rather than fresh Mem0 research

## Shared decisions & glossary deltas   (living — every task must honor these)
- **Gap-analysis-first is the gate.** No downstream build task is created until `mem0-gap-analysis` lands and its P0/P1/P2 ranking is human-confirmed. Provisional tasks below are materialized from the analysis via the open-loop (loop.md), not pre-committed.
- **Built ≠ wired.** Every gap claim and every fix must show the *production* path exercises it (discriminating test / live probe), per the foundation's built-not-wired discipline — "a primitive exists" is never evidence it is used on the hot path.
- **No stale-memory parity claims.** Mem0 capabilities are re-verified against current Mem0 sources (web research), not the assistant's Jan-2026 cutoff.
- **Audit-not-rebuild for existing assets.** `lunaris-bench` evals and `lunaris-core::circuit_breaker` are the canonical implementations; tasks extend/wire/benchmark them.

## Shared / risky contracts (freeze these first)
- gap-analysis taxonomy + P0/P1/P2 ranking rubric (the dimensions audited, the evidence standard per claim, the prioritization criteria) -> owning task `mem0-gap-analysis`

## Tasks (breadth-first decomposition; detail lives in each TASK.md)
- [x] mem0-gap-analysis   depends-on: none   — DONE (gate PASS 2026-06-14). `docs/competitive/mem0-gap-analysis.md` + ranked backlog; validator-gated; 3 load-bearing findings code-verified.
Wave 1 (spawned from the confirmed analysis via loop.md, Tin Dang 2026-06-15 — "Full P0 + all P1 wave"):
- [x] io-failsafe-wiring             P0  depends-on: none  — **DONE (gate PASS 2026-06-15)**. Half B of the split: per-op response timeout on every Moon command via `TypedClient::connect_with_timeout(.., moon_op_timeout())` (env `LUNARIS_MOON_OP_TIMEOUT`, default 10s) so a stalled HSET/FT.SEARCH/TXN can't hang the handler. Discriminating integration test (`tests/op_timeout.rs`, handshake-then-stall fake server) verified red→green (502ms redis-default → ~3s configured); pure `parse_op_timeout` unit test; clippy/fmt clean; dependent `lunaris-memory` compiles. Residual: re-confirm `connect_with_timeout` in the crates.io-published moondb 0.2.1 at next release (non-blocking).
- [x] extractor-fallback-wiring       P0  depends-on: io-failsafe-wiring  — **DONE (gate PASS 2026-06-15)**. Half A: `default_extractor()` candle cache-hit arm now wraps the real extractor via new `lunaris_extract::fallback::fallback_wrap(e, "gemma-3-4b-it")` → `FallbackExtractor`+`CircuitBreaker` on the production open() path (no longer test-only). Behavioral seam tests (transient→Noop, terminal→propagate) + structural `include_str!` wiring guard, red→green; clippy/fmt clean. Honest limitation (approved @ freeze): real candle arm needs ~3GB weights, not CI-runnable → wiring proven by seam test + source guard. **io-failsafe is now whole (Half B + Half A).**
- [x] observability-rollout-maturity P1  depends-on: none  — **DONE (gate PASS 2026-06-15)**. Additive `StoragePort::health_check` (default `Ok`, Moon overrides with a `PING` bounded by `LUNARIS_MOON_OP_TIMEOUT`) → `Lunaris::health_check` → `healthz_handler` returns **503 `{ok:false}`** on a dead/stalled Moon (emits both 503 status AND body so the rollout cutback can key on either). Boot-time `eval_score::load_eval_scores_from_env` publishes `lunaris_eval_score{harness}` from `eval-results.json` (soft-fail on missing/malformed). Discriminating handler→503 test + 3 eval-score tests + Moon structural guard, red→green; clippy/fmt clean; workspace compiles (all backends use the additive default). Residuals: Moon PING proven by guard+op_timeout bounding; eval publish is boot-only; PG `health_check` defaults to Ok (SELECT 1 = PG-parity follow-up).
- [ ] eval-gauntlet-ci-gate          P1  depends-on: none  — un-stub LoCoMo/LongMemEval/ER-F1 (today hardcode 0.0); make CI fail on a seeded sub-threshold regression
- [ ] memory-update-intelligence     P1  depends-on: none  — write-time contradiction/dedup so memories converge (Mem0 ADD/UPDATE/DELETE/NOOP parity); ingest is append-only today
- [ ] multi-level-memory-categories  P1  depends-on: none  — typed user/session/agent levels + categories + metadata-filter API over Scope
- [ ] sdk-integrations-dx            P1  depends-on: none  — LangGraph/CrewAI adapters (currently absent though docs claim them); makes the adapter claim true
- [ ] mem0-docs-reconcile           —   depends-on: none  — quick-fix: correct stale Mem0 claims in POSITIONING.md / why-lunaris.md (adapters "shipped" → roadmap; Mem0 graph "n/a" → Mem0g exists)
Deferred (P2, not in wave 1): graph-quality-parity · correctness-security-harden — Lunaris is already AHEAD on both; "lock-in" polish, schedule after wave 1.

## Exit criteria (observable; map each to the task that delivers it)
- [x] `docs/competitive/mem0-gap-analysis.md` exists with a ranked, code-verified P0/P1/P2 backlog; ranking human-confirmed (Tin Dang 2026-06-15)   (← mem0-gap-analysis)
- [ ] every confirmed **P0** gap ships green via a created, verified task                                                   (← io-failsafe-wiring)
- [ ] the eval gauntlet runs as a real CI **gate** (not just a job) with documented Mem0-comparable numbers                 (← eval-gauntlet-ci-gate)
- [ ] every external IO path (Moon · embedder · extractor LLM · verifier LLM) provably uses timeout+retry+circuit-breaker via a discriminating test  (← io-failsafe-wiring)
