# Changelog

All notable changes to Lunaris are documented here.
Entries before 0.6.0-rc.1 are preserved raw in [docs/CHANGELOG-archive.md](docs/CHANGELOG-archive.md).

## [Unreleased]

### Added

- **Per-scope retention (W4.6, D6.4).** `ScopedLunaris::set_retention_policy` /
  `retention_policy` / `enforce_retention` (and `enforce_retention_at` for a
  caller-pinned cutoff). A policy is `{ max_age_ms, hard }`; rows whose
  valid-time start is older than `now - max_age_ms` are eligible.

  Retention is **opt-in per scope** — a scope with no policy is never swept.
  The failure mode of an accidentally-applied policy is unrecoverable data
  loss; the failure mode of an accidentally-absent one is disk. A policy that
  does not parse is an error rather than `None`, because `None` means "keep
  everything" and a typo'd field would otherwise disable retention silently.

  A sweep is an ordinary scoped `ForgetTarget::Before`, not a direct delete, so
  it inherits `forget`'s soft/hard semantics, its chunk sweep, its
  single-`atomic_write` invariant, and its audit receipt. A hard sweep still
  goes through the D-21 confirmation rail — it runs the preview and derives its
  token from that receipt, exactly as a human hard-forget must. The policy is
  the standing authorization, not a bypass.

  Lunaris does **not** schedule sweeps. A library the host embeds does not get
  to start a background thread that deletes data on a timer; the surfaces can
  expose `enforce_retention` and the scheduling belongs to the deployment.

- **The audit trail can be read back (W4.6, D6.3 — G2).**
  `ScopedLunaris::audit_events(from_ms, to_ms, limit)` returns an `AuditPage`
  of decoded `AuditEvent`s from the bound scope's own trail. Before it, a
  `grep` for a consumer against `__lunaris_audit__` returned producers only —
  every surface wrote events and nothing read them, so "who deleted this?",
  the question the trail exists to answer, had no answer.

  The read is **non-destructive**. `subscribe` is a consumer: it pops, acks,
  and hands each entry over exactly once, so reading a trail through it would
  destroy the trail and a second reader would find nothing. This goes over the
  underlying stream instead, mutating nothing, so an operator can run the same
  query repeatedly and a background subscriber on the same topic is
  unaffected. New additive `StoragePort::queue_range` method (default
  `NotSupported`, same contract as `queue_depth`), implemented by the Moon
  backend.

  Entries that fail to decode are counted in `AuditPage::undecodable` rather
  than skipped in silence — a reader that quietly drops what it cannot parse
  reports "no records" for a topic that has them.

- **Dropped audit events are countable (W4.6, D6.3 — G3).**
  `lunaris_core::audit::audit_events_dropped()` counts every audit event that
  was produced but never reached the broker, and `lunaris-server` exports it
  as `lunaris_audit_events_dropped_total` on `/metrics`.

  Fire-and-forget is unchanged and deliberate — a broker hiccup must not roll
  back a `forget` that already committed. What changes is that the loss stops
  being invisible: "we have no record" and "it did not happen" were the same
  observable state. The counter lives in `lunaris-core` as a plain atomic
  rather than a prometheus metric, because core carries no metrics dependency
  and the drop happens on every surface (MCP, hook, CLI, HTTP), not only the
  one that serves `/metrics`; the server mirrors it in at scrape time.
### Fixed

- **The scratchpad's time-travel read returned rewritten, stale, or no content
  (F42).** `AsOfScratchpad::read` — and the multi-chunk fallback in
  `CodingSessionMemory::read` — rebuilt the answer by concatenating `Hit::text`
  across every hit. Three independent defects, all measured on a live Moon:

  - **Content was rewritten at ingest.** Chunk text is a lossy projection of
    the document: the chunker parses with `pulldown_cmark::Options::all()`,
    which includes `ENABLE_SMART_PUNCTUATION`, and rebuilds text from the event
    stream, so `--` became an en dash, `...` became `…`, ASCII quotes became
    typographic ones and spaces appeared inside values. (Filed as
    "analyzer-normalised index text"; the search index was never involved.)
  - **Superseded versions were glued onto the answer.** Every write mints a new
    Episode under the same `source`; all versions stay indexed and all were
    concatenated, so the answer accumulated stale content in proportion to how
    often the path had been edited.
  - **A stopword-like path was write-OK, read-IMPOSSIBLE.** A path whose name
    analyses to an empty FT query (`big`, `state`) made the read fail outright
    with `ERR empty query after analysis`.

  Both callers now route through a single `WorkingMemory` read, which recovers
  the value verbatim from the parent Episode and resolves to one version; the
  second reconstruction path is deleted rather than repaired.
  `CodingSessionMemory::read` was already correct and is unchanged in behaviour.

  **One behaviour change reaches a shipped surface.** `WorkingMemory::read`'s
  ranked fallback — used by `memory.scratchpad_read` (MCP / contextd) when the
  F40 source-index sidecar is missing — took the top-RANKED hit, which is not
  the same as the CURRENT one: all versions of a key stay indexed under the
  same `source`, so a better-ranked older body could win. It now takes the
  greatest episode ULID, which is the newest version. Reads that were returning
  a stale value in that fallback will start returning the live one.

  Historical `as_of` pins are unaffected: more than an hour back they are still
  refused with `StorageError::NotSupported` on a backend with no KV version
  chain, which is the same guard the old path hit.

### Fixed

- **A repeated soft `forget` re-stamped rows it had already tombstoned
  (W4.6).** `scan_matches_scoped` filtered on the target predicate alone, with
  no check for rows already sys-closed, so every pass re-wrote every row it had
  ever matched and reported them again in `matched` / `rows_written`.

  As a one-shot `forget` that was the cosmetic "`matched` over-count on
  soft-deleted records" recorded as an open follow-up. Retention is what makes
  it matter: a policy sweep runs on a schedule, so a scope with a policy grew
  its write volume without bound while reporting work that changed nothing.
  The scan now drops already-closed rows on the SOFT path only — a hard forget
  must still be able to delete a tombstoned row, because "already hidden" is
  not "already gone".

### Changed

- **Audit events are published under the scope that produced them (W4.6,
  D6.1).** **Breaking, source-level:** `lunaris_core::audit::Publisher::publish`
  and `publish_audit_event` both take a `&Scope` as their new first/second
  argument. Callers outside this workspace that implement `Publisher` or call
  `publish_audit_event` need one added parameter; there is no wire-format
  change and no data migration.

  The old signature had no scope, so the blanket `impl Publisher for Arc<dyn
  StoragePort>` supplied `Scope::dev()`. Moon namespaces MQ topics per scope
  (`lunaris:{scope}:__lunaris_audit__`), so **every** audit event in the
  system — forget receipts, consolidator promotions and archives, verifier
  arbitrations, reflect invalidations — was filed against the `dev` partition
  no matter which tenant produced it. Two consequences: a tenant reading their
  own audit stream, the only stream they are entitled to read, saw nothing for
  operations that definitely happened; and whoever could read `dev` could read
  everybody's receipts.

  All six production call sites now forward the scope they already held. The
  two exceptions are deliberate and marked: `Lunaris::forget` (the deprecated
  unscoped entry point) and `run_consolidate_worker` (the deprecated
  single-topic wrapper) both audit under the same `Scope::dev()` their reads
  and writes already use — auditing anywhere else would file a receipt against
  a partition the operation never touched. Their superseders,
  `ScopedLunaris::forget` and `ConsolidateSupervisor`, carry real scopes.

  This is the hard prerequisite for the D6 governance work: a read API over a
  dev-scoped stream would have served one tenant another tenant's history.

- **Recall ranking on the MCP / hook / CLI path shifts toward previously-used
  memories (F43).** The ACT-R activation prior was **inert**: `boost_prior`
  clamped negative activation to zero, and because activation is
  `ln(weighted * elapsed^-decay)` with `elapsed` in **seconds**, it read
  exactly `0.0` for any memory older than `weighted²` seconds — 9 seconds
  after one strong reference, 15 minutes after ten. `BOOST_CAP` (0.30) needed
  a weighted sum of 1.944e8 and was never approached.

  Replaced with `BOOST_CAP * logistic((activation - BOOST_MIDPOINT) /
  BOOST_SCALE)`, which is monotone over the whole ACT-R range. For a memory
  referenced ten times, the prior added to a fused score becomes:

  | age  | 10s | 1min | 10min | 1h | 6h | 1 day | 7 days |
  |---|---|---|---|---|---|---|---|
  | before | 0.118 | 0.086 | 0.019 | **0.0** | **0.0** | **0.0** | **0.0** |
  | after  | 0.204 | 0.183 | 0.155 | 0.133 | 0.111 | 0.096 | 0.076 |

  **What operators need to know.** No data migration: `ActivationRecord` is
  unchanged on disk, and no reindex is required. Existing ledger rows keep
  their contents — only their *effect* changes, from approximately nothing to
  a real rank prior. Concretely, after upgrading:

  - `memory.recall` (MCP), contextd, the Claude Code hook and `lunaris-cli`
    will order results differently, favouring memories that have been
    referenced before. This is the documented intent of the feature; it is
    the first release in which it actually happens.
  - **Any saved recall baseline or ordering snapshot taken on that path needs
    re-taking.** A diff against an older baseline is expected, not a
    regression.
  - HTTP `/v1/recall` and the Rust/Python/TypeScript SDKs **are affected too**
    as of W1.8 (below), which moved the wiring to the one recall-builder
    construction site every surface goes through. In the same release they go
    from never applying the prior to applying the corrected one.
  - `LUNARIS_ACTIVATION_BOOST=0` remains the supported opt-out and is itself
    unchanged: it removes the boost provider entirely, so no prior is applied
    at all. Note it is **not** a rollback to the previous *on* behaviour — the
    old curve still contributed a small boost inside its cliff window (~0.118
    for a memory referenced ten times, ten seconds ago). Setting it to `0`
    matches the old ordering everywhere EXCEPT for memories referenced in the
    last seconds-to-minutes, where the old code did rank them up.

  **No published benchmark number changes.** `lunaris-bench` never calls
  `record_activation_refs`, so every gauntlet run reads an empty ledger and
  every prior is `0.0` both before and after.

  `lunaris_core::activation::BOOST_K` is `#[deprecated]` (kept, not removed,
  so the public surface does not shrink); `BOOST_MIDPOINT` and `BOOST_SCALE`
  replace it, with `BOOST_SCALE` as the tuning dial. See
  `docs/book/src/reference/configuration.md` for the full curve.

  *At release: this entry needs a corresponding note in the version's
  `docs/migration/` guide — it changes observable ordering for existing
  deployments.*

- **The activation prior now applies on every recall surface, not just
  `lunaris-memory-service` (W1.8).** The SDK has shipped
  `ScopedLunaris::record_activation_refs` — a public method for writing
  reinforcement signals — since the activation-ledger task. Its read half was
  wired in exactly one place, `lunaris-memory-service`, so an SDK, HTTP or
  hook caller could write reinforcement signals that its own recall would
  never read back. That is a write half with no read half, not a defaults
  preference, so the wiring moved to the single recall-builder construction
  site (`crates/lunaris/src/recall.rs`) that every surface reaches through
  `dsl()`.

  - Newly affected: HTTP `/v1/recall`, the Rust/Python/TypeScript SDKs, and
    hook context injection. Their ordering now favours previously-referenced
    memories, exactly as MCP `memory.recall` already did.
  - Unchanged in behaviour, changed in location: MCP, contextd and
    `lunaris-cli` reach the same wiring through the shared builder.
    `lunaris_memory_service::recall::with_activation_boost` is gone; nothing
    outside the crate called it.
  - **Cost:** one activation-ledger point read per distinct hit, per recall,
    on every surface — bounded by the hit set, never by the scope's size, and
    pinned by `ledger_read_cost_is_one_point_read_per_distinct_hit`. Callers
    with no ledger rows pay the reads and get an empty result;
    `LUNARIS_ACTIVATION_BOOST=0` removes them.
  - `LUNARIS_ACTIVATION_BOOST=0` is the opt-out on every surface, and now has
    a test of its own (`tests/activation_boost_optout.rs`) rather than being
    documented-only.
  - **No published benchmark number changes**, for the same reason as F43:
    `lunaris-bench` never calls `record_activation_refs`, so the gauntlet
    reads an empty ledger on both arms. Teaching the bench to populate the
    ledger is tracked separately — until it does, any A/B on the prior is
    comparing two identical arms.

## [0.7.1] — 2026-08-25

### Added

- **Lunaris owns its Moon install path (W0.10).** `scripts/install-moon.sh`
  installs the Moon server Lunaris requires, walking a ladder and failing only
  if every rung fails: reuse a Moon already on this machine, else a published
  release tarball for the pinned tag, else a shallow clone of the public source
  tag plus `cargo install --path`. `setup-lunaris-agents.py` now **runs** it
  when no Moon is found rather than printing a command (`--install-moon never`
  restores the old behaviour).

  This replaces ten surfaces — every install guide, three Rust error strings,
  and the agent setup's own no-Moon exit — that told users to run Moon's curl
  one-liner. **That command could not succeed on any machine:** it downloads a
  release tarball and, with no `VERSION`, resolves to the *latest* tag, and
  Moon v0.8.5, v0.8.6 and v0.8.7 all publish zero binary assets while the ghcr
  image is private. Lunaris is no longer gated on another project's release
  pipeline; re-cutting those assets now only makes installs faster.

  The version is pinned to the engine's `MIN_MOON_VERSION`, never "latest".
  `MOON_VERSION=` pins forward and is validated against that floor rather than
  trusted; `INSTALL_DIR=` chooses the target.

### Fixed

- **`scripts/bump-version.sh` left `crates/lunaris-ts/package-lock.json`
  stale**, which has broken two releases: `npm ci` refuses a lock that
  disagrees with `package.json`, so every workflow installing lunaris-ts dies
  at that step. v0.7.0's fix landed reactively after nightly failures. The bump
  now rewrites the lock's version, its `optionalDependencies`, and the
  `node_modules/@pilotspace/lunaris-*` resolution entries.

### Known limitations

- **The Moon binary exposes no version flag.** `moon --version` and `moon -V`
  are both rejected, and the version is not recoverable from the binary; the
  only surface is `INFO server`, which requires a running server. An arbitrary
  Moon therefore cannot be version-checked offline. `install-moon.sh` records
  what it installs in a `.moon-version` marker and reuses unmarked binaries
  with a warning, relying on Lunaris' connect-time check as the real gate.


### Fixed

- **A hard `forget` left the content recallable (W1.4).** `forget` only ever
  matched the `episode:` row. A SOFT forget looked correct because `hydrate`
  drops any chunk whose parent episode is sys-closed — but a HARD forget
  deletes that episode row, so the gate's lookup returned `None` instead of
  `Some((_, true))`, stopped firing, and the chunk hydrated again with an empty
  `source`. The irreversible, confirmation-token-gated path was the leaky one.
  `forget` now reaches the matched episodes' chunk rows: soft stamps them, hard
  deletes them, and neither depends on the episode row surviving.

### Changed

- **`ForgetReceipt.rows_written` / `.rows_deleted` now count ROWS, not
  episodes.** Because a forget reaches chunk rows, `forget(Id)` on a
  one-chunk episode reports `2` where it previously reported `1`. The fields
  are named for rows and the old number under-reported what was written; a
  caller asserting an exact count should assert `>= 1` (or `> 1` to require
  that chunks were included). Behaviour of the forget itself is otherwise
  unchanged, and `dry_run` still writes nothing.

  **`ForgetReceipt.matched` and the MCP `memory.forget` response are
  unaffected** — both still count EPISODES, which is what their names and docs
  say and what a caller confirming a deletion reasons in. An earlier draft of
  this change let the row count leak into both; CI caught it, and the counts
  are now derived separately rather than sharing one number that happened to
  agree until chunks were swept.

### Known limitations

- A hard `forget` does **not** remove the chunk's vector embedding or BM25
  text from Moon's FT index. Recall is clean (a hit whose chunk row is gone is
  dropped at hydration), but the bytes remain, and `classify_indices` still
  reports `Kv + Vector + Graph` affected when only KV rows are written.
- Facts, entities, relations and communities are **not** forgotten with their
  episode, and cannot be under the current schema: they are keyed by a content
  hash (`FactId = blake3(subject‖predicate‖object)`), so two episodes asserting
  the same thing share one row, and no provenance links a row back to the
  episodes that contributed it. Doing this correctly needs contributing-episode
  provenance plus reference-counted deletion.

## [0.7.0] — 2026-08-18

Moon-only, and the GA cut. Every storage backend except Moon is deleted,
the recall pipeline is one root across every surface, and the release
gates that used to be aspirations are executed artifacts: a recall-quality
ratchet that runs in CI, a measured latency envelope at the target corpus,
and a §7 upgrade/rollback procedure that has actually been rehearsed.

### Added

- **One production recall root.** `lunaris_retrieve::production_root(k,
  graph)` — `Vector ∧ BM25("chunks")` (∧ graph fact legs when enabled) →
  RRF(60) → top-k — is now what HTTP/SDK `recall`, MCP `memory.recall`,
  and the Claude Code hook all execute. Named conformance pins assert the
  plan shape per surface (`plan_repr`), so the benched path and the
  shipped path can no longer drift apart silently.
- **Opt-in reranker stage in production recall.** `LUNARIS_RECALL_RERANK=1`
  inserts the bge-reranker cross-encoder over the fused candidates
  (`LUNARIS_RECALL_RERANK_TOP_IN`, default `2k`) on HTTP/SDK and MCP.
  Default OFF — and the capacity study is why: ~1.3 s/recall at
  `top_in=60` vs a ~20 ms un-reranked recall. OFF provably never loads
  the reranker GGUF. The hook's 1.5 s-budget path never reranks.
- **recall-ratchet CI gate.** Judge-free LongMemEval-S any-gold hit-rate
  (N=16, 4-shard CPU matrix) against a committed baseline
  (`scripts/bench/lme/baselines/ci-anygold.json`, 15/16, tolerance 1,
  config-signature-locked). Replaces the Eval Gauntlet, which had 20
  startup failures and 0 completed runs; the workflow guard now pins the
  ratchet instead. First green run reproduced the Darwin baseline
  exactly (same single miss) on Linux CPU.
- **Measured capacity envelope** (`docs/operations/capacity.md`): at the
  100k-doc target corpus the production root holds the core contract —
  p50 19.2–22.4 ms, p99 ≤ 24.4 ms (25 ms p50 ceiling, ≤25% headroom).
  Graph-ON fact legs measure ~39 ms p50 (2× cost); rerank ~1.3 s/recall.
  Raw envelopes committed under `docs/benchmarks/ga2b-raw/`.
- **Operations pack**: SLO document with multiwindow burn-rate alert
  rules (`docs/operations/slo.md`, `deploy/prometheus/lunaris-alerts.yml`),
  `SECURITY.md` + book security page, a ~45-row environment-variable
  reference, and a CJK-query planner pin (vector-only, no dead BM25 leg).
- **Release engineering**: RELEASE.md §7 upgrade/rollback is stamped
  **Rehearsed on 2026-08-18** (both directions, live, including the
  post-migrate re-embed step it was missing); the drill ships as
  `scripts/release/rollback-drill.sh`.
- **Decision records**: graph fact-legs stay default-OFF for GA
  (latency contract + no measured quality win;
  `docs/decisions/2026-08-18-graph-default-off-ga.md`); OTLP export is
  post-GA (`docs/decisions/2026-08-17-otlp-post-ga.md`).
- **PersonaMem benchmark results** published in README + book. *(Corrected
  2026-08-21, W3.9: this entry read "32k v1: 63.3% / 53.5% — above
  TencentDB-Agent-Memory's published claim". That comparison was wrong —
  63.3% is **below** Tencent's published 76%, and the entry cited a
  superseded v1 run with no operating point stated. The current published
  figures are the 32k split's **75.0% single-reader / 41.9% no-memory
  floor on the `quality` operating point**, with their caveats, in
  [`scripts/bench/pm/RESULTS.md`](scripts/bench/pm/RESULTS.md). Neither
  is a head-to-head with Tencent — see
  [`docs/benchmarks/README.md`](docs/benchmarks/README.md#competitor-figures).)*

### Changed (recall defaults)

- The HTTP/SDK default recall root was a bare `Vector("chunks", 30)`;
  it is now the fused `production_root` — BM25 + RRF join the default
  path. MCP's `with_root` override no longer silently discards graph
  fact legs. The "sub-25 ms over millions" headline is scoped to the
  measured 100k-doc envelope until a larger corpus is measured.

### Fixed

- `h2` 0.4.13 → 0.4.16 (RUSTSEC-2026-0258, unbounded empty DATA frames).
- Bench scratch-Moon probes no longer depend on `redis-cli -t` (a
  redis-cli ≥8 flag): version-agnostic wrapper, fixes all-shard CI
  failures on ubuntu-24.04's redis-tools 7.0.15.

### Removed — BREAKING

- **The Postgres backend (`lunaris-storage-postgres`) and the embedded SQLite
  backend (`lunaris-storage-embedded`) are deleted.** `postgres://`,
  `postgresql://`, `sqlite:///path` and `memory://` all return
  `StorageError::UnsupportedScheme` from `lunaris::open` / `Lunaris::open`,
  with a message naming the migration tool, the release it ships in, and
  `docs/migration/0.6-to-0.7.md`. `moon://host:port` is the only scheme left.
  The `sqlx` workspace dependency went with them — Lunaris no longer links a
  SQL driver at all.
  - `lunaris::PostgresStorage` and `lunaris::{BootstrapReport,
    bootstrap_app_role}` are gone from the public API.
  - `lunaris-server` loses its `migrate` and `bootstrap-db` subcommands;
    invoking either prints why and exits 2. Moon needs neither a schema
    migration nor a role bootstrap — point `--storage` at `moon://host:port`
    and serve.
  - The `pg-it` cargo feature is removed from every crate that declared it.

- **`lunaris-migrate` is deleted from the workspace.** It cannot be built from
  `main` — its source backends are gone. Operators moving 0.6.x data into Moon
  MUST run it **from the v0.6.2 release binary, before upgrading**. See
  `docs/migration/0.6-to-0.7.md` for the procedure and the lossy-conversion
  contract. `lunaris-storage-postgres` is removed from the crates.io publish
  list; the 0.6.2 versions stay on crates.io for anyone still on the old
  backends.

- **`LUNARIS_TEST_BACKEND=memory` is removed.** It selected the embedded
  backend, which no longer exists. The value is now a named hard error rather
  than a silent upgrade to Moon: every `lunaris-test-harness` fixture runs
  against a disposable child-process Moon, and a missing `moon` binary PANICS
  with a diagnostic naming `MOON_TEST_BINARY` instead of degrading. CI builds
  `vendor/moon/target/release/moon` before `cargo test --workspace`
  accordingly. `LUNARIS_TEST_BACKEND` unset / blank / `auto` / `moon` are all
  accepted and equivalent.

- **No binary silently picks a store any more.**
  - `lunaris-mcp` had a per-scope `sqlite:///<HOME>/.lunaris/<scope>.db`
    default. It is now a startup refusal carrying the external-Moon
    quickstart; `--storage` / `LUNARIS_MCP_STORAGE` is mandatory. The
    `embedded-moon` feature is unaffected (dev/test-only, never in `default`).
  - `lunaris-hook` resolved the same file as its third step, after
    `LUNARIS_STORE_URL` and contextd Moon discovery. Those two stay; the
    fallback is now `ScopeResolveError::NoStoreUrl` with the same quickstart.
  - `lunaris-bench`'s ER-F1 harness defaulted to `memory://`. `MOON_URL` (or
    `LUNARIS_URL`) is now required — deliberately with no replacement default,
    since a bench that guesses can guess a live store.

- **The eight `lunaris-recipes` Moon-vs-Postgres parity test files are
  deleted**, along with `lunaris-conformance`'s `run_storage_postgres.rs`,
  `run_storage_embedded.rs`, the four EVAL-05 `sqlx` regression tests, and the
  live half of `run_as_of_parity.rs`. The unconditional STORE-07 gap pin
  survives as `run_as_of_moon_gap.rs`. CI's `pg-lunaris` service, the
  `integration-vanilla-pg-negative` job, and the `postgres` row of
  `conformance-bindings` are removed with them — the bindings parity matrix
  now builds and runs a real Moon instead of neutral-skipping.

### Changed

- **Docs say Moon, everywhere a reader starts.** `README.md`, the book's
  getting-started / MCP / operations / cookbook / migrating chapters,
  `docs/ARCHITECTURE.md`, `docs/POSITIONING.md`, and the three
  `docs/integration/*.md` guides no longer open with `memory://`, tell you to
  run `sqlx migrate run`, or offer `--storage-backend sqlite`. The book page
  "Choosing a Backend (Moon vs Postgres)" is now "The Storage Backend (Moon)"
  and leads with the exit ramp; "Querying Three Ways (Zero-Deps SQLite)" lost
  the premise and kept the content.
- **The `examples/quickstart-rs` compose file runs Moon**, not
  `postgres:16` + pgvector + AGE + pgmq, and the example reads
  `LUNARIS_STORE_URL` with no default. `scripts/pg-lunaris/` and
  `docs/ci/postgres-integration.md` are deleted — they built the image and
  the CI job that image served.
- **STORE-07 is now a flat limitation, not a backend choice.** Every place
  that said "historical KV reads work on Postgres/SQLite" says instead that no
  0.7.0 backend keeps a KV version chain, so `read_as_of` with a historical
  pin refuses with `501 not_supported`. The search and graph lanes stay
  temporal. `docs/release/deprecations.md` gains a §3 for the two deleted
  storage crates (tombstone README, do **not** yank — every `lunaris-memory`
  through 0.6.2 depends on them).

## [0.6.2] — 2026-08-15

The operability release: first non-rc cut of the 0.6 line, and the last
release in which the Postgres and SQLite backends ship (both are removed
in 0.7.0 — see `docs/release/deprecations.md` and
`docs/migration/0.6-to-0.7.md`).

### Changed

- **BREAKING (Moon backend) — historical `read_as_of` / `scan_range` now
  fail loudly instead of returning present-time data.** Moon stores Lunaris
  rows as plain hashes: `HGET`/`HMGET` accept no `AS_OF` clause and an
  overwrite destroys the prior value, so there was never a historical version
  to return. Until now the adapter answered a past `as_of` with the *current*
  row, which made `GET /v1/snapshot/{lsn}`, `POST /v1/recall {as_of: <past>}`
  and `AsOfScratchpad::read` hand back fabricated history. Such requests now
  return `StorageError::NotSupported` → HTTP `501 { "error":
  "not_supported" }`. Latest-state reads — every recall, hydrate, forget,
  verify and detail lookup — are unchanged, and Moon's search/graph lanes stay
  temporal via `FT.SEARCH AS_OF` / `GRAPH.QUERY VALID_AT`. As-of KV reads
  remain available on Postgres and SQLite. Upstream path to closing the gap:
  Moon's unwired `TemporalKvIndex` (`record`/`get_at`).

- **MCP `memory.forget` previews by default (0.6.2 Task F)** — the tool's
  request DTO gained a `dry_run` field that **defaults to `true`**. Omitting it
  now scans and reports instead of deleting; an actual delete requires an
  explicit `"dry_run": false`. Before this change the DTO carried
  `deny_unknown_fields` and no `dry_run` field at all, so an LLM could issue an
  irreversible scope-wide delete and had no way to preview one. The response is
  now `{ status, dry_run, matched, removed }` (flat struct — the rmcp
  `outputSchema` root-object invariant forbids an enum tag). The HTTP
  `POST /v1/forget` surface keeps `dry_run: false` as its default for API
  compatibility; only the MCP surface inverts it.

- Docs narrowed accordingly: the bi-temporal **write** model still holds on
  every backend; as-of **reads** are now documented as Postgres/SQLite for KV
  and Moon-native for the search + graph lanes (README, `docs/ARCHITECTURE.md`
  § Honest limits, book concepts / data-structures / introduction /
  conformance, the three migration guides, `docs/guide.md`,
  `docs/helios-integration.md`, `docs/POSITIONING.md`).

- **HTTP server hardening** — graceful shutdown now drains in-flight
  requests with a bound; requests carry a 30s timeout, a 256-request
  concurrency limit with load-shedding, and `/readyz` performs a real
  storage write canary instead of a liveness echo.

- **moondb pin → 0.3.0** — `vendor/moon` moves to moon main `d70bebbd`
  whose SDK removes five dead wire forms (none used by Lunaris) and
  registers `TXN`/`FT.AGGREGATE`. The server still reports
  `moon_version 0.8.5`, so the connection handshake floor is unchanged.

### Added

- **`lunaris-migrate`** — one-shot Postgres/SQLite → Moon migration CLI
  with an explicit lossy contract: system-time history collapses to
  migration time, only open-interval rows migrate, and vector/BM25/graph
  lanes stay dead until re-embedding (`--reembed-manifest`). Dry-run by
  default; a real run requires `--acknowledge-lossy`. Ships in the 0.6.x
  line only and is deleted in 0.7.0.

- **Moon version handshake** — the Moon adapter now reads the
  `moon_version` INFO field at connect and refuses servers older than
  0.8.5 (fail-open when the field is unrecognizable, fail-closed on
  transport faults).

- **Multi-shard fail-fast** — connecting to a Moon started with
  `--shards N>1` is now a hard, deterministic startup error (read-only
  `MULTI`/`EXISTS` probe). Sharded Moon cannot yet host Lunaris writes or
  graph recall; see RFC 0008 (`docs/rfcs/0008-sharded-moon-ingest.md`).

- **`lunaris-test-harness`** — child-process ephemeral Moon for the test
  suite (`LUNARIS_TEST_BACKEND=auto|moon|memory`, `MOON_TEST_BINARY`);
  the workspace suite now runs against a real Moon in ~3ms boots instead
  of only the in-memory port.

- **Operations docs** — external-Moon onboarding, observability guide,
  Docker Compose + Dockerfile under `deploy/`, and a rehearsed
  backup/restore drill (`scripts/backup-restore-drill.sh`,
  `docs/operations/backup-restore.md`; RPO 0, RTO < 1s at test scale).

- **Release integrity** — crates-publish now verifies the vendored moondb
  SDK source matches the crate published at the same version
  (`scripts/check-vendored-moondb-parity.sh`) and guards crate
  publishability metadata (`xtask` publish-metadata tests);
  `lunaris-hook` is explicitly unpublished.

- The LME benchmark harness moved into version control
  (`scripts/bench/lme/`) with a guard test ratchet; bench runs use a
  dedicated Moon port and hard-refuse the production store.

### Fixed

- **Moon keyword hits scored 0.0** — the Moon SDK parser strands
  `__bm25_score` in the returned field map; the adapter now recovers it,
  so BM25 hits fuse with their real scores instead of flattening to zero.

- **`ForgetReceipt.matched`** — the number of primitives the target matched,
  populated on every path including `dry_run`, where `rows_written` and
  `rows_deleted` are both zero by construction. Without it a preview could not
  tell the caller what a commit would remove. Additive and
  `#[serde(default)]`, so receipts minted by older servers (the HTTP
  `confirmation_token` carries a serialized prior receipt) still deserialize.

- `StoragePort::supports_historical_kv_reads()` — new additive trait method
  (default `true`; Moon overrides to `false`) so callers can route as-of reads
  instead of discovering the hole at query time. Deliberately distinct from
  `StorageCapabilities::bi_temporal_native`, which means "temporal reads are
  *native*" and is `false` on Postgres even though Postgres answers them.
  Pinned by the non-skipping conformance test
  `read_as_of::historical_pin_is_explicit` (runs for every backend) and
  `moon_declares_its_as_of_gap` (runs with no live services at all).

## [0.6.0-rc.2] — 2026-07-17

Second release candidate — fixes two P0-class SDK defects found while
re-validating rc.1 (silent zero-vector recall in the shipped wheels/binaries,
and a deterministic crash at Python process exit), unifies the Moon processor
inside `lunaris-contextd`, and bumps the vendored Moon to v0.8.0.

### Fixed

- **SDK zero-vector P0 (PR #61, `48ec406`)** — `lunaris-py` / `lunaris-ts`
  never forwarded the `llamacpp` (and `metal`/`cuda`/`vulkan`) features to the
  umbrella `lunaris` crate, so every shipped wheel / `.node` binary silently
  fell back to `NoopEmbedder`: default `open()` returned all-zero vectors and
  hybrid recall ranked by BM25 + tie-break only. Both manifests now forward
  the features, pinned by a manifest guard
  (`crates/lunaris-core/tests/sdk_feature_forwarding.rs`) and proven at
  runtime (real semantic scores; recovery TESTs 1–3 pass on the fixed wheel).
- **Python exit crash (PR #64)** — any Python worker that loaded the
  llama.cpp embedder aborted with SIGABRT at normal process exit
  (`GGML_ASSERT([rsets->data count] == 0)` in ggml-metal's static
  destructor). `lunaris-llamacpp` now parks engine state in a takeable
  teardown registry; the Python package auto-registers
  `shutdown_inference()` with `atexit`, so Metal buffers are freed before
  C++ static destructors run. Post-teardown calls return a typed `Closed`
  error. A subprocess regression test asserts exit code 0 after a real embed.
- **Legacy `codex:*` feedback leak (PR #65)** — `excluded_context_source`
  exact-matched the four `lunaris:*` lifecycle literals only, so episodes
  stored before the 2026-07-14 source-prefix rename leaked
  `codex:turn_feedback` / `codex:memory_injection` records into prompt
  injections. The predicate now matches the lifecycle kind for any origin
  prefix; negatives pin that `tool_call` / `decision` / `edit` sources stay
  injectable.
- **Installer Moon identity probe (PR #63)** — `setup-lunaris-agents.py`
  defaulted to port 6380 (the ai-proxy Redis on some boxes); it now defaults
  to 6381 and verifies the endpoint actually speaks Moon (PING + `FT._LIST`)
  before wiring hooks, rejecting a foreign Redis with an actionable error.
- **`LUNARIS_EMBED_BATCH` no longer latched forever (PR #63, closes #49)** —
  the ingest batch-size env override is re-read on every call instead of
  cached in a once-init static.
- **Codex adapter fail-open (`2e475b7`)** — missing hook binaries no longer
  hard-fail the codex adapter.
- **Inference watchdog (`0deb6c4`)** — wedged Metal embeds are bounded; the
  hook exits 70 to self-heal instead of hanging the session.

### Changed

- **contextd embedded-Moon unification (PR #62)** — the Moon processor now
  runs inside the `lunaris-contextd` process (discovery file + loopback-only
  RESP-PING probe); the hook path no longer needs a separately launched Moon.
- **Vendored Moon v0.7.1 → v0.8.0** (plus the dashtable recovery fix); the
  recovery harness now probes the MQ / temporal / graph planes and the #69
  upgrade-replay mode.

### CI / build

- Workspace rustdoc warnings zeroed out (58 sites) and
  `release-preflight.sh` refreshed for the post-v0.6 publish set; the
  three v0.6-era publishable crates (`lunaris-llamacpp`,
  `lunaris-embed-remote`, `lunaris-llm`) now ship READMEs.
- npm pre-release publishes route to the `next` dist-tag.
- The eval-gauntlet workflow is `workflow_dispatch`-only while the
  self-hosted runner pool is empty (a guard test pins the trigger set).
- deps: crossbeam-epoch 0.9.20 (RUSTSEC-2026-0204), spin 0.9.9 (yanked
  upstream).

## [0.6.0-rc.1] — 2026-07-15

First release candidate for 0.6.0 — the **llama.cpp-only inference cutover**
plus five closed milestones. Bundled milestones (attribution in
[RELEASES.md](RELEASES.md)): moon-v030-exploit · claude-code-flagship ·
memory-contract-integrity · hook-session-scratchpad · memory-inspector.

### Added

- **Native-graph hybrid recall (`FT.NAVIGATE`)** — the retrieval DSL fuses
  semantic + BM25 + one-hop graph traversal in a single Moon round trip, so
  linked facts surface without a second query. Recall p50 ~3.9ms @10k.
- **RAPTOR tree retrieval** — `.tree(index, k, depth)` operator plus
  community summaries embedded at ingest, for hierarchical recall over long
  corpora (proven wired and traversed end-to-end).
- **SQ8 scalar-quantized vectors (opt-in)** — `?quant=sq8` handles cut vector
  storage ~4x with recall@k held at the 0.90 floor (CI-gated).
- **Claude Code turnkey install** — a two-command setup wires Lunaris memory
  into a Claude Code session (hybrid recall on the prompt hook, capture on
  tool-use / session events). Session start injects a prior-session digest.
- **Session handover as a first-class event** — switching agent sessions
  drains the working-memory pad and promotes durable facts; a per-session
  scratchpad keeps ephemeral state out of long-term recall.
- **Memory Inspector** — a read-only local dashboard to pick a scope and
  browse episodes, entities, and relations at rest.
- **Proxiable scratchpad + engine ops** — the four `memory.scratchpad_*`
  tools and the engine ops route through `contextd`'s warm per-scope engine
  over a unix socket (with a circuit-breaker fallback to a direct in-process
  engine), so a socket-mode MCP needs no second model load.
- **Framework store adapters** — LangGraph `BaseStore`, CrewAI
  `BaseRAGStorage`, and Letta archival connectors over the memory protocol.
- **Write-time convergence** — deterministic sync dedup of facts plus
  cross-episode contradiction detection with bi-temporal supersede.

### Changed

- **llama.cpp is the sole local inference backend (BREAKING).** The candle
  embedder/reranker/LLM stack is deleted; the embedder
  (granite-embedding-311m, Q4_K_M GGUF) and reranker (bge-reranker-v2-m3,
  Q5_K_M GGUF) now run in-process via `llama-cpp-2`. Extractor / verifier LLM
  slots are **remote-only** (`LUNARIS_EXTRACT_PROVIDER` /
  `LUNARIS_VERIFY_PROVIDER`). SDK entry points are
  `EmbedderConfig.llamacpp()` / `RerankerConfig.llamacpp()`; the retired
  `native()` factories raise a migration hint. Feature `llamacpp` is the
  umbrella default (needs cmake + a C++ toolchain); `default-features = false`
  builds a Tier-0 no-inference binary. GPU is a build-time choice
  (`metal` / `cuda` / `vulkan`), replacing the previous candle Accelerate /
  Metal auto-defaults. See `docs/migration/0.5-to-0.6-llamacpp-only.md`.
- **Priority-lane embedder** — recall-query embeds jump ahead of in-flight
  ingest batches, so a background ingest never blocks an interactive recall.
- **License: Apache-2.0 only** (was dual `Apache-2.0 OR MIT`).

### Fixed

- **contextd socket transport was dead for all memory ops** — the MCP proxy
  framed requests without the `type` discriminator contextd's `ContextRequest`
  decode requires, so every socket call silently fell back to a direct engine.
  Requests are now framed correctly and covered by a wire round-trip test.
- **contextd scope bleed** — the daemon resolved scope from its own birth-env,
  so every repo's captures collapsed into one scope; scope is now resolved
  per-request.
- **Recall filter push-down + hybrid filter gap** — `Filter` now pushes into
  both hybrid branches on Moon (array-category membership matched correctly).
- **Supersede loser is now closed on real backends** — a contradicted fact's
  bi-temporal interval is closed on Moon/Postgres, not just SQLite.
- **11-pattern secret scrubber** on captured hook content (keys, tokens,
  passwords) before it reaches long-term memory.

### Performance

- Single-round-trip `read_as_of`, concurrent hydration fan-out, batched
  `scan_range`, and query-embedding reuse across the two-leg `ThenRetriever`
  reduce recall latency across the Moon storage path.
