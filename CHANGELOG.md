# Changelog

All notable changes to Lunaris are documented here.

## v0.2.1 — 2026-05-11 — Scope alphabet hardening (RC-2 closure)

Patch release that closes RC-2 from the v0.2.0 release-gate review: the
scope validation regex no longer permits `:`. The `lunaris:{scope}:{kind}:{ulid}`
KV format is now unambiguous at the type level — no scope string can
byte-alias another scope's per-kind SCAN prefix.

### Breaking

- **`Scope::new` rejects `:`.** The validation regex tightens from
  `^[A-Za-z0-9_\-:.]{1,128}$` (v0.2.0) to `^[A-Za-z0-9_\-.]{1,128}$`
  (v0.2.1). Any v0.2.0 caller that minted scope strings containing `:`
  (e.g. `acme:agent-1`) must rewrite to `.` or `-` (e.g. `acme.agent-1`)
  before upgrading. The hand-rolled `Deserialize` re-validates wire input,
  so v0.2.0 JWTs or request payloads with colon-containing tenant claims
  will now fail at the HTTP boundary with `invalid scope`.
- **Postgres CHECK constraint tightens to match.** Migration
  `20260512000007_scope_regex_tighten.sql` drops + recreates the
  `<table>_scope_check` constraint on `episodes`, `chunks`, `entities`,
  `relations`, `facts`, `communities`, and `lunaris_kv`. **Operators
  with v0.2.0 data containing `:` in scope strings MUST rewrite those
  rows before applying the migration** — the `ADD CHECK` step otherwise
  aborts with a constraint-violation per row. Recipe in the migration's
  header comment:
  ```sql
  UPDATE episodes    SET scope = replace(scope, ':', '.');
  UPDATE chunks      SET scope = replace(scope, ':', '.');
  UPDATE entities    SET scope = replace(scope, ':', '.');
  UPDATE relations   SET scope = replace(scope, ':', '.');
  UPDATE facts       SET scope = replace(scope, ':', '.');
  UPDATE communities SET scope = replace(scope, ':', '.');
  UPDATE lunaris_kv  SET scope = replace(scope, ':', '.');
  ```
  Run inside a transaction, then `sqlx migrate run`.

### Fixed

- **RC-2 — scope prefix delimiter ambiguity closed at the type level.**
  v0.2.0 allowed `Scope::new("a:episode")` — that scope's KV prefix
  `lunaris:a:episode:` byte-aliased `Scope("a")`'s episode-kind SCAN
  prefix on Moon, enabling cross-scope SCAN bleed. v0.2.1 rejects the
  colon form at the validating constructor, so the structural invariant
  is now compiler-enforced: for any valid scope, `scope_prefix(&s)` and
  `<kind>_prefix(&s)` cannot alias because the kind suffix is the only
  segment containing `:`. The previously-`#[ignore]`'d regression test
  `keyspace::scan_prefix_does_not_alias_across_kinds` is now active and
  pins the contract.

### Added — OSS ship-to-product surface (Phases 20–24)

This release also lands the bulk of the OSS-foundation work tracked in
`tmp/lunaris-ship-to-product-v2.md`. Every item below is additive
unless flagged otherwise.

- **Workspace versioning** — every crate now inherits
  `version.workspace = true` from `[workspace.package]` so a single
  bump propagates across all 18 crates.
- **`[workspace.dependencies]` centralisation** — every internal
  `lunaris-*` dep flows through one declaration with `path` + `version`,
  unblocking `cargo publish`. Member crates use `{ workspace = true }`.
- **`#[non_exhaustive]` on growable public enums** — `LunarisError`,
  `StorageError`, `ExtractError`, `ValidateError`, `RetrieveError`,
  `ConsolError`, `PublishError`, `AuditEvent`, `IndexKindData`,
  `ScopeSpecData`, `ForgetTargetData`, `WriteOp`, `Filter`,
  `ForgetTarget`, `ScopeSpec`, `IndexKind`. Downstream `match` sites
  add wildcard arms with `NotSupported` / "unknown" labels.
- **crates.io manifest hygiene** — 8 publishable crates gain
  `description`, `repository.workspace = true`, `readme`, `keywords`,
  `categories`, plus a per-crate `README.md` stub. `cargo publish
  --dry-run` on `lunaris-core` is now warning-free.
- **`publish = true`** on 8 ready crates (`lunaris-core`,
  `lunaris-storage-postgres`, `lunaris-embed`, `lunaris-rerank`,
  `lunaris-extract`, `lunaris-verify`, `lunaris-consolidate`,
  `lunaris-ingest`). The 3 moondb-blocked crates wait on the sibling
  Moon repo to publish — `docs/RELEASE.md` §3 documents the
  resolution path.
- **RFC 0006 scaffold** — `crates/lunaris-verify/src/candle_gemma3_270m.rs`
  behind the new `verify-small` feature. Mirrors the 27B impl with
  laptop-floor constants (~540 MB RAM target). Production default-flip
  is gated on the Phase 24 head-to-head bench + the 100-item quality
  gate from RFC 0006 §4.
- **RFC 0006 backend selector** — `LUNARIS_VERIFIER_BACKEND` env var,
  resolved by `default_verifier()` in `crates/lunaris/src/handle.rs`.
  Values: `270m` / `small` (RFC 0006 laptop floor, default with
  `verify-small`), `27b` / `large` (legacy default, opt-in via
  `verify-large`), `noop` (operator opt-out), anything-else →
  `tracing::warn!` + `NoopVerifier`. Cache-miss on either Candle
  backend falls back to `NoopVerifier` per the D-02 default-OFF
  contract — identical to the `default_extractor` shape. Umbrella
  `lunaris/Cargo.toml` now forwards `verify-small` + `verify-large`
  so callers can opt in without depending on `lunaris-verify` directly.
- **`LICENSE`** at the repo root (Apache-2.0; matches the `license`
  field every Cargo.toml declared since v0.1.0).
- **`Makefile`** with `make bench-public`, `make ci-local`, and
  `make test-pg` / `make test-moon` reproducibility targets (Phase 24).
- **`docs/RELEASE.md`** — concrete release runbook for v0.2.x cuts:
  TL;DR shell flow, pre-flight checklist, SemVer discipline,
  publishable-surface table, multi-platform wheel + .node matrix,
  rollback procedure, open questions.
- **`examples/quickstart-rs/`, `quickstart-py/`, `quickstart-ts/`**
  — three-language 10-minute scaffolds against a shared docker-compose
  Postgres image. Phase 23.
- **README rewrite** — first 30 lines now answer the OSS reader's
  "what is this and why should I use it" question. The internal
  milestone-phase progress moves to `CHANGELOG.md` / RFCs.
- **RFCs opened (Draft)**: RFC 0004 (`ExtractorTier` typestate),
  RFC 0006 (Verifier 27B → 270M default swap), RFC 0007
  (`FallbackExtractor`/`FallbackEmbedder` combinators).
- **RFC 0001 amendment §11** — as-shipped closure for the v0.2.0 +
  v0.2.1 release-gate review.

### Fixed — additional v0.2.1 closures

- **P-2 — `Lunaris::forget` warn-on-non-dev-scope.** Emits
  `tracing::warn!` at the call site documenting that the forget path
  still routes through `Scope::dev()` until the v0.3 typed surface lands.
- **P-3 — supervisor `register_scope` TOCTOU.** Placeholder-oneshot
  reservation under a single write-lock; `ConsolidateSupervisor` and
  `VerifySupervisor` close the race between fast-path check and
  idle-timeout deregistration.
- **P-5 — propagation matrix extended.** `scoped_lunaris.rs` regression
  tests now cover `graph_traverse`, `read_as_of`, `publish`,
  `subscribe`, `scan_range` in addition to `vector_search`.

### CI gates added

- `ingest_04_single_atomic_write` — grep gate at
  `crates/lunaris-ingest/src/pipeline.rs` asserting exactly one
  `storage.atomic_write` call site. Core-value enforcement.
- `cargo check -p lunaris-verify --features verify-small` — keeps the
  RFC 0006 scaffold from rotting.
- `cargo check -p lunaris-verify --features verify-large` — symmetry
  alias for the 27B path.
- `cargo_semver_checks` — Phase 20 gate against the `v0.2.0` baseline
  for `lunaris-core` + `lunaris`.
- `cargo_publish_dry_run_core` — Phase 22 gate for the leaf of the
  publish dep graph.

### Known issues / v0.3 carryover

- `Lunaris::forget` is still hard-coded to `Scope::dev()` (P-2 emits a
  `tracing::warn!` at the call site; `ScopedLunaris::forget` is a v0.3
  deliverable).
- Pipeline handles still use deprecated single-topic workers (carryover).
- Postgres production deployments must use a `NOSUPERUSER NOBYPASSRLS`
  role (operational, not a code bug).
- `lunaris-storage-moon`, `lunaris-retrieve`, and the `lunaris` umbrella
  crate are NOT yet on crates.io — they transitively depend on `moondb`
  (path-only sibling repo). Resolution: publish `moondb` upstream, then
  flip `publish = true` on the three.

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
- **RC-A — Postgres `keyword_search` did not set `lunaris.scope` GUC.**
  Found during target-review of v0.2 vs the "Sub-25 ms recall" Core
  Value (`tmp/v0.2-target-review.md`). Every other PG read path wraps in
  a read tx + `SET LOCAL lunaris.scope`; `keyword_search` queried the
  pool directly. Under the documented `NOSUPERUSER NOBYPASSRLS` role,
  `FORCE ROW LEVEL SECURITY` then filtered every row out for any
  non-`_legacy` scope — BM25 silently returned zero hits in production.
  The bug was masked because `tests/scope_isolation.rs` covered
  `vector_search` + `read_as_of` but not `keyword_search` under the
  app role. Fixed: wrap the BM25 query in the same tx + `set_config()`
  pattern as `vector.rs`. New live regression test
  `cross_scope_keyword_search_returns_zero_for_wrong_scope`.

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
