# Changelog

All notable changes to Lunaris are documented here.

## v0.4.0 — 2026-06-13

### Performance

Quick task `260610-f91`: removed the serialized storage round trips from the
recall hot path with Moon as the default backend. Measured A/B at k=30 (MCP
stdio, live Moon): recall p50 12 → 6.0 ms, p99 97.3 → 6.2 ms — the tail no
longer scales with k. Methodology: `docs/benchmarks/v0.6-recall-fanout-ab.md`.

- **`read_as_of` is one round trip** (`lunaris-storage-moon`): the two serial
  `HGET`s (`v`, then `bt`) collapsed into a single `HMGET key v bt`. Every KV
  read on the Moon backend — hydration, scratchpad, recovery — pays half the
  round trips.
- **Concurrent hydration fan-out** (`lunaris-retrieve`): `hydrate()` and
  `partial_hydrate_text()` (the rerank candidate feed) now issue their
  per-hit reads via an ordered `buffered(32)` stream instead of one awaited
  read per hit; episode lookups fan out with `buffer_unordered(32)`.
  Concurrent requests pipeline over the shared multiplexed Moon connection,
  so a recall-with-rerank pays a handful of batches instead of ~120
  sequential round trips. Hit order is preserved; guarded by
  Barrier-deadlock concurrency tests (`tests/hydrate_concurrent.rs`).
- **`scan_range` per-batch fan-out** (`lunaris-storage-moon`): the per-key
  `HGET`s within each `SCAN` batch run concurrently (bounded 32), preserving
  batch order — speeds up working-memory list/grep paths.
- **`ThenRetriever` reuses the query embedding**: the narrowed second-leg
  context is seeded from the parent's computed embedding instead of re-running
  the embedder forward pass (the query text is unchanged by `.then(...)`;
  only the filter narrows).

### Fixed

- **Episode-read errors propagate from concurrent hydration**: the fan-out
  rewrite initially swallowed storage errors in the episode pass (silently
  degrading `Hit::source` to `""`); restored the pre-fan-out `?` contract and
  pinned it with `hydrate_propagates_episode_read_errors`.

### Added (RAPTOR tree retrieval)

Completes the RAPTOR retrieval path. The hierarchical community tree that
adaptive chunking builds at ingest is now both **populated as a searchable
vector index** and **traversable from the retrieval DSL**, closing the
"built-but-never-traversed" gap. (This is the Moon-exploit enhancement batch on top of v0.6.)

- **`.tree(index, k, depth)` retrieval operator** (`lunaris-retrieve`) — RAPTOR
  hierarchical retrieval over the `communities` index. Vector-searches the `k`
  nearest community summary nodes, then descends `Community.members` breadth-first
  (depth `1..=MAX_TREE_DEPTH` where `MAX_TREE_DEPTH = 4`, default `1`) to collect
  every leaf chunk beneath them — including chunks that fall outside a flat
  search's top-k budget. One vector search per query; deeper levels only read
  community KV rows, so latency scales with depth × fan-out, not index size.
  Composes with `.and()` / `.or()` / `.then()` / `.fuse_rrf()` / `.top()` like
  every other operator. Operator form: `Tree::new("communities", k)`; builder
  shortcut: `RetrievalBuilder::tree(...)`. Re-exported as `lunaris::Tree`.
  Documented in `docs/book/src/guides/retrieval-dsl.md`.

- **Community summaries embedded at ingest** (`lunaris-ingest`) — the RAPTOR
  community tree's `summary` nodes (built bottom-up during ingest) now carry a
  populated `summary_embedding` (768-d), written into the `communities` vector
  index in the **same** ingest write batch (one extra `embed_batch`, no new
  model, INGEST-04's single `atomic_write` preserved). Previously
  `summary_embedding` was always `None` and the `communities` index was empty —
  so `.tree()` and `Vector::new("communities", k)` returned nothing. This is the
  precondition that makes tree retrieval functional.

#### Notes

- **Validation scope.** Tree retrieval is proven *wired and traversed* by a
  discriminating integration test against live Moon: on a whole-document query,
  flat top-`k` returns a single chunk while `.tree()` returns the full leaf set
  (≥ 2). Semantic relevance-vs-flat with a production embedder is **not yet
  benchmarked** — tracked as a follow-on, not claimed here.

- **Deferred this batch** (detail in `tmp/moon-exploit/FOLLOW-ONS.md`):
  Moon-native graph hybrid (`FT.NAVIGATE`) and SPLADE learned-sparse 3-way RRF
  are both blocked on Moon **server** features (no sparse-write wire path;
  `FT.NAVIGATE` degrades to plain KNN on current graph data) and are deferred
  behind a filed Moon feature request. The bi-temporal KV hydrate gap has a
  completed design spike (Lunaris-side versioned-key, ~4.5 days) pending
  implementation.

### Added (MCP working memory + embedded Moon)

Merged the `feat/mcp-scratchpad-tools` milestone (PR #19): a working-memory
scratchpad surface for the MCP server, an opt-in in-process Moon, and a
guarded on-demand consolidate tool.

- **Four `memory.scratchpad_*` MCP tools** (`lunaris-mcp`) — key-addressed
  working memory under a `scratchpad/` namespace, separate from the durable
  episode log. `scratchpad_write` / `scratchpad_read` are KV put/get,
  `scratchpad_grep` lists entries by key-prefix, and `scratchpad_consolidate`
  drains the scratchpad queue and promotes/archives notes by ActR activation.
  The MCP server now registers **eleven** tools (seven durable + four
  scratchpad).

- **`memory.scratchpad_consolidate` — guarded on-demand consolidation**
  (`lunaris-mcp`) — reuses `WorkingMemory::consolidate()` behind three guards:
  it requires a native-queue backend (returns `{ status: "unsupported_backend" }`
  on SQLite/memory), refuses to run while a background consolidation worker is
  live (`{ status: "worker_conflict" }`), and bounds one drain to a 5 s
  wall-clock cap (`{ status: "timeout" }`).

- **`--features embedded-moon` (opt-in)** (`lunaris-mcp`) — when built with the
  feature AND no `LUNARIS_MCP_STORAGE` override is set, `lunaris-mcp`
  auto-launches an in-process Moon (`moon::server::embedded::run_embedded`,
  rooted at `./.lunaris-moon`) and uses it; on bring-up failure it falls back
  to the SQLite default. The feature is **off by default** and is NOT compiled
  into the published `npx` / `uvx` / `cargo install` binaries, so the shipped
  MCP storage default remains SQLite.

- **`server_boot.rs` integration test** (`lunaris-mcp`) — spawns the real
  binary, drives the `initialize` → `tools/list` handshake over stdio, and
  asserts all eleven tools register. Closes the built-vs-wired gap one level
  above the handler logic (the unit tests never construct the rmcp tool
  router, so a green unit suite did not prove the server can start).

### Fixed (MCP startup)

- **`lunaris-mcp` could not start (any build).** `ScratchpadConsolidateResponse`
  was a `#[serde(tag = "status")]` enum, whose generated MCP `outputSchema`
  root is `oneOf` (no `type`). rmcp 1.7 validates each tool's `outputSchema`
  when building the tool router and aborts startup (exit 101) on a non-object
  root — so the server was un-launchable. Fixed by making the response a flat
  struct (root `type: "object"`); guarded by a `schema_for!` root-is-object
  regression test and the new `server_boot.rs` boot test. (`89b9181`)

## v0.5 Wave D — npx + uvx distribution: DIST-01 npm package + DIST-02 PyPI package (2026-05-26)

### Added

- **`@pilotspace/lunaris-mcp` npm package** (DIST-01) — `npx @pilotspace/lunaris-mcp` installs and runs
  `lunaris-mcp` without a Rust toolchain. Postinstall downloads the platform-native
  binary from the GitHub Release (5 platforms: `linux-x64`, `linux-arm64`,
  `darwin-x64`, `darwin-arm64`, `win32-x64`) and verifies sha256 against
  `manifest.json` before extracting via `tar(1)`. Binary is cached in the npm package;
  subsequent invocations exec the binary directly. `LUNARIS_MCP_BIN_PATH` env var
  provides an air-gap bypass. `docs/integration/claude-code.md` gains a "Quick Start
  (no Rust)" section.

- **`lunaris-mcp` PyPI package** (DIST-02) — `uvx lunaris-mcp` installs and runs
  `lunaris-mcp` without a Rust toolchain. Per-platform wheels (`manylinux_2_28_x86_64`,
  `manylinux_2_28_aarch64`, `macosx_12_0_x86_64`, `macosx_12_0_arm64`, `win_amd64`)
  each carry the prebuilt binary in `lunaris_mcp/bin/`. `uvx` resolves the correct
  wheel by platform tag. `pip install lunaris-mcp` also supported. PyPI trust anchor:
  TLS + per-file hash verification (PEP 427). Canonical registry: `pypi.org`.
  `docs/integration/codex.md` gains `uvx lunaris-mcp` as the primary no-Rust path.

## v0.5 Wave C — MCP polish: record_decision / record_edit aliases + CodingSessionMemory rename (2026-05-25)

### Added

- **`memory.record_decision` MCP tool** — structured alias over `memory.ingest` for
  capturing architectural and scoping decisions. Input: `{ decision, rationale,
  alternatives?, tags?, dedupe_key? }`. Writes Episode with
  `source = "decision:<scope>"` and metadata `{"kind": "decision", "tag_count": N}`.
  Documented in `docs/integration/claude-code.md` and `docs/integration/codex.md`.

- **`memory.record_edit` MCP tool** — structured alias for capturing file edits.
  Input: `{ path, before?, after, intent?, dedupe_key? }`. Writes Episode with
  `source = "edit:<scope>"` and metadata `{"kind": "edit", "path": "<value>"}` —
  enabling future `memory.recall` filter-by-path queries.

### Changed

- **`HeliosScratchpad` renamed to `CodingSessionMemory`** (MCP-03). The
  `crates/lunaris/src/recipes/helios_scratchpad.rs` file is renamed to
  `coding_session_memory.rs`; the `pub struct HeliosScratchpad` type is renamed
  to `pub struct CodingSessionMemory`. This broadens the recipe's audience from
  Helios specifically to any coding agent (Claude Code, Codex, etc.) that needs
  a filesystem-shaped session memory surface.

### Deprecated

- **`HeliosScratchpad`** — now a `pub type HeliosScratchpad = CodingSessionMemory`
  alias with `#[deprecated(since = "0.5.0")]`. v0.4 consumers continue to compile;
  they will receive a `deprecated` compiler warning. The alias will be **removed in
  v0.7**. Migration: replace `lunaris::HeliosScratchpad` with
  `lunaris::CodingSessionMemory` at every call site.

## v0.5 Wave B — `lunaris-hook` scaffold + Codex parity decision (2026-05-25)

First `lunaris-hook` binary release: proactive capture of Claude Code lifecycle
events into Lunaris memory via stdin envelope → `ScopedLunaris::ingest` → exit.

### Added

- **`lunaris-hook` binary crate** — reads one Claude Code hook envelope from
  stdin, derives scope from `cwd + git remote` (same algorithm as
  `lunaris-mcp`), writes one Episode, exits 0 on success or non-zero with
  a structured error. Four event kinds: `PreToolUse`, `PostToolUse`, `Stop`,
  `SessionStart`. Unknown kinds exit 0 (forward-compat no-op; exit 66
  reserved for Phase 24 filter-rejected events). See
  `docs/integration/hooks.md`.
- **`lunaris-core::scope_resolver`** — scope derivation lifted from
  `lunaris-mcp` into `lunaris-core` via `ScopeStore` trait. Both
  `lunaris-mcp` and `lunaris-hook` produce bit-identical scopes for the
  same repo without copy-paste drift. Each binary owns its own
  `JsonScopesFileStore` impl pointing at `~/.lunaris/scopes.json`.
- **Hook integration guide** — `docs/integration/hooks.md` with event kind
  table, exit codes, environment variables, and Phase 24 deferred items.
- **HOOK-07 ADR** — `docs/decisions/2026-05-25-codex-hook-deferral.md`
  documents the primary-source finding (no public Codex hook API as of
  2026-05-25) and defers Codex parity to a future follow-up phase.

### Scope of `lunaris-hook` v0.5

Phase 24 ships filter policy, secret scrubber, dedupe key, and the cold-start
latency gate (p50 ≤ 50ms). Phase 23's hook binary intentionally omits these
to keep the scaffold atomic and the envelope schema settled.

## v0.4.0-wave-a — lunaris-mcp publish (2026-05-24)

External-agent integration surface. Wave A delivers `lunaris-mcp`, a
binary crate that speaks MCP over stdio so Claude Code, OpenAI Codex,
and any other MCP-native agent can register Lunaris as a memory server.

### Added

- **`lunaris-mcp` binary crate** — `cargo install lunaris-mcp` then add to
  your editor's MCP config. Four tools: `memory.ingest`, `memory.recall`,
  `memory.forget`, `memory.list_scopes`. Stdio transport, no auth
  (process-bound). See `docs/integration/claude-code.md` (5-step walkthrough)
  and `docs/integration/codex.md` (Codex CLI parity guide).
- **Lazy GGUF model stager** — Q4_K_M embedder + Q5_K_M reranker download to
  `~/.lunaris/models/` on first vector recall (stderr progress bar, sha256
  verify, idempotent). `tools/list` responds in <500 ms (CI gate at
  `crates/lunaris-mcp/tests/cold_start.rs`). No model files downloaded until
  `memory.recall` is first called.
- **Scope resolver** — derives a stable `lunaris_core::Scope` from
  `git remote.origin.url + branch` (blake3 hash → `"git_<hex16>"`), with
  canonical-cwd fallback (`"cwd_<hex16>"`). Persisted at
  `~/.lunaris/scopes.json`; users may rename scopes manually. CLI
  `--scope` / `LUNARIS_MCP_SCOPE` overrides the derivation.
- **INGEST-04 invariant gate for the MCP entry point** —
  `crates/lunaris-mcp/tests/atomic_write_gate.rs` enforces that
  `memory.ingest` goes through `ScopedLunaris::ingest`, never a direct
  `atomic_write`. Mirrors the existing `lunaris-ingest` grep gate.
- **Integration docs** — `docs/integration/claude-code.md`,
  `docs/integration/codex.md`, decision record
  `docs/decisions/2026-05-24-claude-code-mcp-reversal.md`.

### Changed

- **`lunaris-storage-embedded` WAL + busy_timeout** — every SQLite connection
  now opens with `PRAGMA journal_mode=WAL`, `PRAGMA busy_timeout=5000`,
  `PRAGMA synchronous=NORMAL`. Two Claude Code windows (or Codex windows) on
  the same repo share the same `~/.lunaris/<scope>.db` safely — WAL allows
  one writer and multiple concurrent readers without `SQLITE_BUSY` errors.
  Regression test at `crates/lunaris-storage-embedded/tests/concurrent_handles.rs`.

### Fixed in Wave A.1

- **SQLite `vector_search` brute-force cosine** (`5674c52`) — closes the
  "Moon/Postgres required for vector recall" caveat. The SQLite backend now
  supports `memory.recall` end-to-end. Brute-force cosine scales to ~10k
  vectors per scope; HNSW (Moon) or pgvector (Postgres) is the right choice
  for larger corpora.
- **`lunaris-codegen` `Graph::anchored` emitter** (`adea496`, `22e8c40`) —
  emits `Vec<(EntityId, f32)>` for weighted seeds; restores
  `cargo check --workspace` green.

### Known limitations

None at Wave A.1 ship. Wave B / C / D items: SSE transport + Bearer auth,
`coding_session_memory` recipe, `npx`/`uvx` distribution, multi-user server
mode — see the Reversed and Deferred sections in the integration guides.

### Reversed

- The "Claude Code FS adapter shape (CAS, mtime, prefix_scan_meta_only)
  inside Lunaris" out-of-scope row referenced from `docs/helios-integration.md:13`
  is reversed. Wave A ships an MCP-shaped surface (`memory.ingest`,
  `memory.recall`, `memory.forget`, `memory.list_scopes`), not an
  FS-tool-shaped one. Filesystem semantics remain a Helios-side concern.
  See `docs/decisions/2026-05-24-claude-code-mcp-reversal.md`.

---

## v0.3.0 — unreleased — Scope enumeration + recipe surface (W1/W2 wave)

Additive minor bump (per the workspace lock's spirit at the actual
baseline). The wave lands the `Lunaris::list_scopes` public surface,
the first two recipe slots in the W1/W2 roadmap, an FT-backed
`invalidate_range` fan-out, and the vendor/moon bump that pulls Moon
`version_token` (#97) + `FT.INVALIDATE_RANGE` (#98) into the binary
consumed by CI.

### Added

- **`Lunaris::list_scopes(prefix, limit, cursor) -> ScopePage`** —
  high-level pass-through to `StoragePort::list_scopes` with
  per-backend support matrix documented on the method. Embedded
  (SQLite) and Moon backends enumerate live scopes via key-parse +
  dedupe; Postgres returns `StorageError::NotSupported` by design
  because cross-scope enumeration would require `BYPASSRLS` which
  the production app role MUST NOT hold (RFC 0001 §6). Helios falls
  back to a caller-supplied scope list when it sees `NotSupported`.
- **`lunaris_core::keyspace::parse_scope_from_key`** — reverse of
  `scope_prefix`, used by both backend `list_scopes` impls.
- **`StoragePort::list_scopes` + `ScopePage`** — trait method with
  `NotSupported` default + paged result type (opaque scope-string
  cursor, Q-U1 lock). Resume is strictly-greater-than the last
  emitted scope; re-passing the terminal cursor returns an empty
  page (idempotent past-the-end).
- **`lunaris-storage-moon::scopes`** — `SCAN MATCH lunaris:{prefix}*`
  with per-batch dedupe into a `BTreeSet`. Lazy SCAN-derived (Q-U2
  lock) — promotion to an eager `set:scopes` SADD index is deferred
  until SCAN proves slow at scale.
- **W1-L1 `lunaris-ingest::schema_gate`** — generalized
  chunk-metadata schema gate that prevents the L9 regression class
  (extractor-introduced unknown keys silently pruned). Red-then-green
  TDD pair (`286971a` red, `a37fd93` green).
- **W2-L1 `lunaris-recipes::documentary::code_feature_card`** — first
  recipe slot: weighted vector + keyword + graph fan-in tuned for
  "what does this feature do" prompts, with deterministic ranking.
  Red-then-green TDD pair (`32dcca5` red, `aac4b55` green).
- **W2-L2 `lunaris::invalidate_range`** — bi-temporal range invalidate
  over the Moon `FT.INVALIDATE_RANGE` primitive (Moon #98). Single
  storage call, closed `[lo, hi]` interval, 6 positional args matching
  the Moon wire contract. Red-then-green TDD pair (`8310305` red,
  `cd8c26c` green).

### Changed

- **`[workspace.package].version`** — `0.2.1 → 0.3.0`. All 23
  internal workspace dep entries bumped in lockstep per the
  workspace comment that mandates sync.
- **`vendor/moon` submodule** — `b24b4d0 → b7a443f` (50 commits;
  Moon main HEAD). Pulls in Moon #97 (`version_token` AtomicU64 +
  FT.INFO/VINFO/GRAPH.INFO exposure) and Moon #98
  (`FT.INVALIDATE_RANGE` delete-by-TAG∩NUMERIC-range, 6 positional
  args, closed `[lo,hi]`). Moon is binary-consumed by Lunaris
  (`cargo build --release --manifest-path vendor/moon/Cargo.toml
  --bin moon` per 14-02 Option B), so the SHA bump does not affect
  Lunaris Rust compilation — it changes only the binary CI runs
  against.
- **`prefix.map_or(true, …) → prefix.is_none_or(…)`** in both
  `list_scopes` backend impls (clippy::unnecessary_map_or).

### CI / build

- **All workflows skip the `.planning` submodule.** The
  `.planning/` git submodule points at the private cross-repo
  `pilotspace/lunaris-docs`. The default GHA `GITHUB_TOKEN` is
  repo-scoped and cannot clone other repos in the org, so
  `actions/checkout@v4` with `submodules: recursive` fails at the
  `.planning` clone, blocking the workflow before any test code
  runs. Four workflows (`integration.yml`, `conformance-bindings.yml`,
  `eval-gauntlet.yml`, `llm-gates.yml`; 6 checkout sites) replace
  `submodules: recursive` with `submodules: false` plus an explicit
  `git submodule update --init vendor/moon` step. `vendor/moon` is
  the public-via-HTTPS submodule that auths via the standard
  `GITHUB_TOKEN`. `ci.yml`'s `submodule-tag-parity` job still uses
  `.planning` intentionally and is gated to release-tag pushes —
  left untouched.
- **`vendor/moon` init is non-recursive.** `vendor/moon/.planning`
  → `pilotspace/moon-docs` is SSH-only and unreachable from CI
  runners. The init step uses `git submodule update --init
  vendor/moon` (no `--recursive`).
- **Post-merge fmt sweep** — `cargo fmt --all` across 6 .rs files
  that the rebase-merge of #6/#7/#8 introduced without fmt
  (`crates/lunaris-core/src/storage/port.rs`,
  `crates/lunaris-ingest/src/schema_gate.rs`,
  `crates/lunaris-recipes/src/documentary/code_feature_card.rs`,
  `crates/lunaris-recipes/tests/code_feature_card_recipe.rs`,
  `crates/lunaris-storage-moon/src/invalidate.rs`,
  `crates/lunaris/src/invalidate.rs`).

### Compatibility

- No on-the-wire breaks vs. v0.2.1. The list_scopes addition is
  additive (new method + default `NotSupported` impl on the trait).
- Postgres operators relying on cross-scope enumeration must
  detect `StorageError::NotSupported` from `list_scopes` and fall
  back to a caller-supplied scope list.

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
