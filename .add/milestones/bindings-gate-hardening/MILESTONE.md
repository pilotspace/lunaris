# MILESTONE: Bindings & Gate Hardening

goal: Every binding/integration gate on main is green and trustworthy — the conformance-bindings workflow runs again, the lunaris-ts suite passes against the shipped v0.4+ API, silent test skips become loud CI failures, and a multi-shard Moon is rejected at connect time instead of mid-ingest.
rationale: intake bucket `sub-milestone` — confirmed by Tin Dang 2026-06-11 ("Sub-milestone: all 4"). Two latent main issues surfaced during the PR #20 review (stale TS specs + conformance-bindings broken since 06-08) plus two folded-convention hardening items form one coherent gate-trust wave; tasks 1↔2 are coupled (specs unverifiable without CI; CI red without specs).
stage: production · status: active · created: 2026-06-11

> SDD living doc for this milestone. Keep it THIN: breadth, shared decisions, and
> exit criteria only — per-task detail lives in each `.add/tasks/<slug>/TASK.md`,
> written just-in-time. Update this doc whenever a task reveals a milestone gap.

## Scope
In (Moon-shaped after the 2026-06-11 moon-only redirect):  conformance-bindings workflow repair (maturin venv) · lunaris-ts stale-spec refresh to the v0.4 native API + v0.2.1 scope alphabet · MOON_IT_REQUIRED env that hard-fails graceful skips in CI (unreachable vs incompatible distinguished) · connect-time shard-count guard in lunaris-storage-moon (fail fast on shards>1, citing the TXN-pin RFC)
Out: fixing the live-PG `cypher(cstring)` conformance failure (live-PG parity stays deferred to HUMAN-UAT per standing decision) · perf-gates repair · the 10k×1k like-for-like bench rerun · Moon-repo TXN BEGIN PIN implementation · dim_configurable within-suite race fix (own task, later) · any new SDK surface

## Shared decisions & glossary deltas   (living — every task must honor these)
- CI-first ordering: the workflow must run before spec-green is claimable in CI — local vitest evidence bridges the gap but the exit criterion is a green CI run.
- Folded conventions apply (CONVENTIONS.md foundation v2): graceful-skip distinguishes unreachable/incompatible; probes/tests launch their own servers where feasible.
- The shipped MCP/SDK API surface is the v0.4+ native one (EmbedderConfig.native/native_quantized/noop; NO fastembed/ollama factories in TS); specs must track the addon, never the reverse.
- Scope alphabet is `[A-Za-z0-9_\-.]{1,128}` (v0.2.1) — any artifact still claiming `:` is valid is a bug.

## Shared / risky contracts (freeze these first)
- conformance-bindings workflow shape (venv strategy: maturin develop vs build+pip install) -> owning task ci-bindings-venv-fix
- MOON_IT_REQUIRED semantics (which suites honor it; skip vs fail taxonomy) -> owning task moon-it-required-gate

## Tasks (breadth-first decomposition; detail lives in each TASK.md)
- [ ] ci-bindings-venv-fix     depends-on: none                 — repair the maturin "Couldn't find a virtualenv" failure in conformance-bindings (red on main since 2026-06-08); all three jobs green
- [ ] ts-specs-v04-refresh     depends-on: ci-bindings-venv-fix — refresh lunaris-ts __test__ specs to the v0.4 native embedder API + v0.2.1 scope alphabet (7 stale failures on main) AND wire the FULL `npm test` suite into conformance-bindings (discovered at task-1 specify: no workflow runs any vitest spec except backend_parity — the stale specs are invisible to CI)
- ~~pg-lunaris-bindings-service~~ SUPERSEDED at freeze (2026-06-11): Tin Dang redirected to Moon-only (PG+SQLite deprecate-first, delete next minor) — the postgres row gets REMOVED in the upcoming moon-only milestone, not repaired; task detached
- [ ] moon-it-required-gate    depends-on: none                 — MOON_IT_REQUIRED=1 turns connect_or_skip into hard failure in CI; distinguish unreachable (skip) from reachable-but-incompatible (always fail)
- [ ] shard-count-guard        depends-on: none                 — MoonClient connect-time probe: fail fast (or loud warn) when the server runs shards>1, citing docs/design/scope-hashtag-txn-rfc.md
- [ ] open-call-latency        depends-on: none                 — SPLIT from ts-specs-v04-refresh build (2026-06-11): every `lunaris.open()` burns ~0.8s pure CPU (wall 807ms/cpu 797ms, warm, empty model cache, success AND parse-failure paths) — root-cause (suspect: per-open construction of something heavy in the v0.6-era pipeline) and restore a fast open; the gil-discipline test's absolute bound died on this and was converted to a ratio assertion in the parent task

## Exit criteria (observable; map each to the task that delivers it)
- [ ] conformance-bindings: feature-build smoke + per-driver parity (moon) green on a main push; all 3 instances past the maturin step  (← ci-bindings-venv-fix)
- [x] conformance-bindings postgres row: resolution moved to the moon-only milestone (row removal with the backend deprecation) — criterion closed as superseded, decided by Tin Dang 2026-06-11
- [ ] `npm test` in crates/lunaris-ts passes 0-fail locally AND in the repaired CI job                          (← ts-specs-v04-refresh)
- [ ] A CI run with MOON_IT_REQUIRED=1 fails loudly when Moon is absent, and moon-it suites stop false-passing  (← moon-it-required-gate)
- [ ] Connecting lunaris-storage-moon to a --shards 4 Moon fails (or warns, per frozen contract) at connect time with an RFC-citing message; --shards 1 unaffected  (← shard-count-guard)
