# Changelog

All notable changes to Lunaris are documented here.

## v0.2.0 — 2026-05-11 — Multi-agent partitioning

First-class multi-agent / multi-tenant isolation via the new `Scope`
newtype. **Breaking change at the v0.1 → v0.2 boundary** — see
`docs/migration/0.1-to-0.2.md` for the upgrade path. No on-the-wire
compatibility with v0.1.

### Added

- **`lunaris_core::Scope`** — validated newtype around `SmolStr` (regex
  `^[A-Za-z0-9_\-:.]{1,128}$`). Cheap to clone, inline up to 23 bytes,
  derives `Ord, PartialOrd` for per-scope supervisor maps. `Scope::dev()`
  is a doc-hidden test/migration helper.
- **`lunaris_core::keyspace`** — storage-agnostic primitive KV key helpers
  (`episode_key`, `chunk_key`, `entity_key`, `relation_key`, `fact_key`,
  `community_key`, `scope_prefix`). Format `lunaris:{scope}:{kind}:{ulid}`.
- **`ScopedLunaris<'a>`** typestate wrapper returned by
  `Lunaris::scoped(scope)`. The bound scope propagates through ingest,
  recall, and the DSL builder.
- **`EpisodeBuilder`** — scope-less Episode payload with `pub(crate)`
  terminal `into_episode`. Only `ScopedLunaris::ingest` can stamp a scope
  onto an Episode — cross-scope ingest is a compile error.
- **`ConsolidateSupervisor` / `VerifySupervisor`** — per-scope worker pools
  with bounded concurrency (`LUNARIS_SCOPE_CONCURRENCY`, default 8) and
  idle-scope timeout (`LUNARIS_SCOPE_IDLE_TIMEOUT_MS`, default 30 min).
  Panic in one scope's task is contained; the scope is re-registered.
- **Postgres backend** — `scope TEXT NOT NULL` column on every primitive
  table + Row-Level Security policies + `SET LOCAL lunaris.scope` per
  transaction. Migration `20260510000005_scope_partitioning.sql` backfills
  pre-existing rows with the reserved literal `_legacy`.
- **Moon backend** — per-scope keyspace prefix `lunaris:{scope}:` + per-scope
  FT / GRAPH / MQ resources. Lazy index init per scope.
  `StorageCapabilities.max_scopes_recommended = 512` reflects Moon's FT
  index soft limit.
- **`lunaris-server`** — `AuthClaims.scope: Scope` (was `tenant: String`),
  parsed from the JWT `tenant` claim via `Scope::new()` (401 on invalid).
  Request bodies use `#[serde(deny_unknown_fields)]`: top-level `scope` or
  `metadata.tenant` overrides return HTTP 422.
- **`docs/multi-agent.md`** — public-facing 5-scenario HTTP UAT contract
  for external consumers (Helios + others).
  `crates/lunaris-server/tests/multi_agent_uat.rs` is the executable
  companion (982 lines, all 5 scenarios green).
- **SDK regen** — `lunaris-py` (PyO3 0.26) + `lunaris-ts` (napi-rs 3.x)
  bindings for `Scope`, `EpisodeBuilder`, `ScopedLunaris`. 14 pytest + 50
  vitest assertions green via `maturin develop` and `napi build`.
- **`docs/rfcs/0001-scope-newtype.md`** — full RFC for this release.
- **`docs/migration/0.1-to-0.2.md`** + `docs/migration/api-diff/` —
  migration guide and full public-API diff dumps (546 lines).

### Changed (breaking)

- **Primitive constructors** — `Episode::new`, `Chunk::new`, `Entity::new`,
  `Relation::new`, `Fact::new`, `Community::new` all take `scope: Scope`
  as the first argument.
- **`StoragePort`** — every partitioning method gains `scope: &Scope` as
  the first argument after `&self`. Eight methods affected: `atomic_write`,
  `vector_search`, `graph_traverse`, `scan_range`, `read_as_of`, `publish`,
  `subscribe`, `queue_depth`. `capabilities()` is unchanged.
- **`KeywordPort::keyword_search`** — gains `scope: &Scope` as the first
  argument (RFC 0001 §3.4 amendment; originally overlooked in Wave 0).
- **`QueryContext`** — carries `pub scope: Scope`. `RetrievalBuilder` gains
  `with_scope(scope)` and is pre-seeded by `ScopedLunaris::dsl()`.
- **HTTP API** — JWT `tenant` claim is now mandatory and validated via
  `Scope::new()`. Request bodies cannot override scope via `metadata.tenant`
  or a top-level `scope` field.

### Deprecated

- **`lunaris_consolidate::run_consolidate_worker`** and
  **`lunaris_verify::run_verify_worker`** — single-topic legacy entry
  points. Use `ConsolidateSupervisor` / `VerifySupervisor` instead. Pipeline
  handles (`ConsolidatorPipelineHandle`, `VerifyPipelineHandle`) continue
  using the legacy workers for backwards compat; migration to supervisors
  is tracked for v0.3.

### Removed

- **`AuthClaims.tenant: String`** — replaced by `AuthClaims.scope: Scope`.
- **`metadata.tenant` override on request bodies** — previously honored
  silently as a tenant key. Now rejected with HTTP 422.

### Fixed

- **`hydrate.rs` key-format regression** — Wave 1C scope-prefixed write keys
  via `keyspace::chunk_key` but the READ path in
  `lunaris-retrieve::hydrate` still used the obsolete non-scoped
  `lunaris:chunk:{ulid}` format. Every graph-anchored recall silently
  returned zero hits. Regression pinned by
  `scoped_lunaris::scoped_recall_propagates_scope_to_vector_search`.
- **`recall_graph_mode::mode_graph_falls_back_to_semantic_with_degraded_when_no_entities`** —
  test fixture used obsolete pre-Wave-2.5B key format with `Scope::dev()`
  while the HTTP path read under the JWT's `tenant="t-1"` scope. Migrated
  to `keyspace::chunk_key(&Scope::new("t-1"))` so writer/reader scopes
  match.
- **RC-1 — `Lunaris::ingest` graph-on path wrote Fact KV rows without the
  scope prefix.** `crates/lunaris/src/ingest.rs` retained a local unscoped
  `fact_key(id)` after the Wave 2.5B keyspace move. Two scopes writing
  facts with the same ULID would overwrite each other on Moon. Replaced
  with `lunaris_core::keyspace::fact_key(&episode.scope, f.id)`; deleted
  the local helper.
- **RC-3 — Postgres RLS policies missing `WITH CHECK`.** Original migration
  declared `USING`-only policies. Per Postgres §5.8, INSERT consults only
  `WITH CHECK`; with both clauses omitted, no row-side scope check fires
  on INSERT. Added follow-up migration `20260511000006_rls_with_check.sql`
  that drops + recreates every `tenant_isolation` policy with both clauses.
- **RC-4 — `serde::Deserialize` for `Scope` did not re-validate.** The
  derived `#[serde(transparent)]` impl accepted any string, bypassing
  `Scope::new`'s regex. Replaced with a hand-rolled `Deserialize` that
  calls `Scope::new` on the wire bytes. The existing
  `scope::serde_rejects_invalid_scope_string` test now asserts rejection
  (was asserting the permissive bug).
- **P-1 — `RecallRequest` and `ForgetRequestDto` missing
  `deny_unknown_fields`.** Closed the wire-side `scope` / `tenant`
  smuggling vector on the two remaining DTOs, matching `IngestBody`.

### Known issues / v0.3 carryover

- **`forget` not yet scoped at the engine layer** — `Lunaris::forget` still
  uses `Scope::dev()` internally. UAT-4 documents the target contract
  (`403/404` on cross-scope forget) as an `#[ignore]`'d test.
  `ScopedLunaris::forget` is a v0.3 deliverable.
- **Pipeline handles still use deprecated single-topic workers** —
  `ConsolidatorPipelineHandle` and `VerifyPipelineHandle` will migrate to
  the supervisors in v0.3 (requires plumbing scope through the handle).
- **`index.d.ts` for `lunaris-ts`** — napi-rs regenerates this file on
  every `napi build`. The `Lunaris.scoped()` declaration is added manually
  post-gen and will be lost on the next full rebuild. Proper fix via
  declaration merging lands in v0.2.1.
- **Postgres production deployments must use a non-superuser role** — RLS
  is bypassed by `rolsuper=t` or `BYPASSRLS`. `docs/migration/0.1-to-0.2.md`
  §6.2 has the role-creation recipe.
- **RC-2 — `scope_prefix` is not delimiter-safe.** The validation regex
  permits `:` in scope strings, which collides with the `:{kind}:`
  delimiter in `lunaris:{scope}:{kind}:{ulid}`. A scope `"a:episode"` aliases
  `Scope("a")`'s episode prefix on Moon SCAN. **Operational guidance for
  v0.2.0:** issuers MUST NOT mint scope strings ending in `:episode`,
  `:chunk`, `:entity`, `:relation`, `:fact`, or `:community`. v0.2.1 will
  tighten the regex to drop `:` entirely. Regression test
  `keyspace::scan_prefix_does_not_alias_across_kinds` is `#[ignore]`'d
  until the regex change lands. Postgres RLS is unaffected (row-level
  scope match is column-bound, not prefix-bound).
- **`Lunaris::forget` is a silent zero-match on real scopes.** The forget
  path still uses `Scope::dev()` internally for `atomic_write`, `read_as_of`,
  and `scan_range`, plus a non-scoped `b"episode:"` prefix scan. Same-scope
  forget under a real (non-`_dev_`) scope returns `rows_deleted=0,
  rows_written=0` with no error. Tracked for v0.2.1 (warn-on-non-dev-scope)
  and v0.3 (`ScopedLunaris::forget`).

## v0.1.2

### Changed

- **BREAKING-LIKE (behavioral default change)**: `ConsolidatorPipelineHandle::default()` now
  wires `ActRConsolidator` instead of `NoopConsolidator`. The three-surface toggle (code / env /
  config) is preserved. To retain the v0.1.1 behavior, set `LUNARIS_CONSOLIDATOR_BACKEND=noop`
  before `Lunaris::open`, or call
  `handle.consolidator_pipeline().set_consolidator(Arc::new(NoopConsolidator))` explicitly.
  See `docs/migration/v0.1.2.md` and Phase 16 plans.
- EVAL-05 `promotion_rate` SLO is now enforced (was informational in v0.1.1), with empirical
  band [0.00, 0.01] calibrated against the deterministic 10K-turn trace on Moon + Postgres
  (6 runs: 3 x Moon + 3 x Postgres; see `milestones/v0.1.2-CONSOL-CALIBRATION/SUMMARY.md`).
- HELIOS-05 and HELIOS-06 SLOs lifted from deferred to validated in PROJECT.md (Phase 17).

### Added

- `lunaris_bench::eval_05_slo` module with `enforce_promotion_rate_slo()` function and
  `PROMOTION_RATE_LOW` / `PROMOTION_RATE_HIGH` constants for CI enforcement.
- `docs/migration/v0.1.2.md` migration guide for downstream consumers.
- `milestones/v0.1.2-CONSOL-CALIBRATION/` with 6-run calibration artifacts and band derivation.
- Criterion `helios_p50` v0.1.2 baseline committed at `milestones/v0.1.2-HELIOS-05-BASELINE/`.
  Moon p50 ≤ 20 ms (budget), Postgres p50 ≤ 25 ms (budget). HELIOS-05 validated.
- SIGKILL chaos 200/200 runs (100 x Moon + 100 x Postgres) with `fsck_all` green on every iteration.
  Evidence at `milestones/v0.1.2-HELIOS-06-RESULTS.json`. HELIOS-06 validated.

## v0.1.1

Released 2026-04-23. See `milestones/v0.1.1-MILESTONE-AUDIT.md` for full details.

## v0.1.0

Released 2026-04-21. See `milestones/v0.1.0-MILESTONE-AUDIT.md` for full details.
