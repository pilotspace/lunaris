<!-- GSD:project-start source:PROJECT.md -->
## Project

**Lunaris**

Lunaris is a production-grade **agent memory engine** built as a pure Rust framework with Python (PyO3) and TypeScript (NAPI) SDKs. It takes raw observations (messages, documents, tool results) from agents, extracts structured primitives using a small local LLM, and stores them in a bi-temporal MVCC store backed by **Moon** (our internal high-performance Redis-compatible substrate) with **Postgres** as a portability proof. Agents query it through a composable retrieval DSL that fuses semantic search, graph traversal, and BM25 keyword lookup.

The audience is internal agent platforms first (we own the substrate), with a public Rust crate / `pip install lunaris` / `npm i lunaris` story to follow. Helios is the first downstream consumer; LangGraph / CrewAI / Letta integration is a second-wave concern.

**Core Value:** **Sub-25ms recall over millions of bi-temporal facts, with provable atomicity and a graph that's opt-in.** If everything else fails, that one performance + correctness contract must hold — it's what differentiates Lunaris from Mem0, Zep, and Cognee.

### Constraints

- **Tech stack — Rust**: edition 2024, MSRV **1.94** (matches Moon to ease cross-repo work)
- **Tech stack — Python**: 3.11+ (PyO3 0.29 baseline)
- **Tech stack — TypeScript**: Node 20+, napi-rs 3.x
- **Backend ordering — Moon first, Postgres second**: explicit inversion of blueprint §5.3, justified by internal-first deployment
- **Latest libraries policy**: tokio latest 1.x, axum ≥0.8, sqlx ≥0.9, candle ≥0.9, tower ≥0.5, tracing ≥0.1, thiserror 2.x, anyhow 1.x, serde latest 1.x, redis 0.32+ (or direct Moon SDK if available in `moon/sdk/`)
- **No duplicate vector / BM25 libs**: Moon native `FT.*` is the canonical implementation; Lunaris does NOT bundle a second HNSW (e.g., `instant-distance`) or BM25 (e.g., `tantivy`)
- **Timeline — 7 calendar days to production rollout**: explicit user override of blueprint §14 (90-day plan). The team takes the risk of compressed validation; mitigated by automated quality gates on every push (LongMemEval, LoCoMo, ER-F1, perf smoke) and progressive rollout (5% → 25% → 100% traffic at lunaris.dev)
- **Real-use-case-at-once testing**: all 10 recipes get integration tests against both Moon and Postgres simultaneously. No "we'll add Postgres later" scope cuts.
- **Atomic commits**: every plan/phase commits incrementally. The Moon repo's `git commit -F tmp/<msg>.txt` pattern is mirrored here per the user's global git rules.
- **File size**: no `.rs` file exceeds 1500 lines (matches Moon's convention); split read/write at 1000 lines.
- **Lock discipline**: `parking_lot::RwLock` over `std::sync::RwLock`; never hold a lock across `.await` (matches Moon's UNSAFE_POLICY.md guidance).
<!-- GSD:project-end -->

<!-- GSD:stack-start source:STACK.md -->
## Technology Stack

- **ML runtime** — `candle 0.10` (CPU). Single backend for embedder +
  reranker + extractor + verifier. **v0.4 N-03 cutover (2026-05-14)** deleted
  the fastembed-rs / ONNX Runtime / production-Ollama paths; see
  `docs/migration/0.3-to-0.4-native-default.md`.
- **Embedder** — `ibm-granite/granite-embedding-311m-multilingual-r2`
  (Apache-2.0, ModernBERT, 768-d, FP16) via `lunaris-embed-native`. Q4_K_M
  GGUF variant available under `--features embedder-gguf` for RSS-constrained
  hosts. Air-gap escape hatch: `lunaris-embed-remote::OllamaEmbedder`
  behind `--features embed-remote` (operator-only, not the supported path).
- **Reranker** — `BAAI/bge-reranker-v2-m3` (Apache-2.0, XLM-RoBERTa,
  cross-encoder, FP32, sigmoid output ∈ [0,1]) via `lunaris-rerank-native`.
  Q5_K_M-imatrix GGUF under `--features reranker-gguf`.
- **Storage** — Moon (Redis-compatible, internal) + Postgres (portability
  proof) + SQLite (`memory://`, `sqlite:///path`, zero-deps onboarding).
- **HTTP** — `axum ≥0.8` + `tower ≥0.5` + `tower-http` + `tracing` +
  `prometheus 0.14` (`/metrics`).
- **Async** — `tokio` latest 1.x + `parking_lot` for sync primitives;
  no `std::sync::*Lock` in workspace code (matches Moon's UNSAFE_POLICY).
- **Errors** — `thiserror 2.x` (library) + `anyhow 1.x` (application).
- **SDKs** — `pyo3 0.26` (Python) + `napi-rs 3.x` (TypeScript). Both
  expose `EmbedderConfig.native()` / `RerankerConfig.native()` as v0.4 entry
  points.
<!-- GSD:stack-end -->

<!-- GSD:conventions-start source:CONVENTIONS.md -->
## Conventions

### Scope (RFC 0001) — multi-agent partition key

- **Keyspace helpers belong in `lunaris-core`, not backend crates.** The
  canonical KV format `lunaris:{scope}:{kind}:{ulid}` is encoded by
  `lunaris_core::keyspace::{episode,chunk,entity,relation,fact,community}_key`.
  Any caller that mints a Lunaris KV key from a local helper is a bug — see
  RC-1 (v0.2 review): `lunaris/src/ingest.rs` retained a local unscoped
  `fact_key` after the Wave 2.5B move and silently produced collision-prone
  keys. Backend crates re-export the helpers; engine and infra crates must
  import them from `lunaris_core::keyspace`.
- **Never derive `Deserialize` on a validated newtype.** `Scope` is the
  canonical example: the derived `#[serde(transparent)]` impl bypasses
  `Scope::new` and trusts the wire. Hand-roll a `Deserialize` that calls the
  validating constructor (see `crates/lunaris-core/src/scope.rs`).
- **`Scope::dev()` is a migration crutch.** It is `#[doc(hidden)] pub` for
  test/migration use only. Any new `Scope::dev()` call site in production
  code is a v0.3 carry-over, not a steady-state pattern; thread the real
  scope through instead.
- **Scope alphabet is `[A-Za-z0-9_\-.]{1,128}` (v0.2.1+).** `:` is rejected
  by `Scope::new` so the `lunaris:{scope}:{kind}:{ulid}` KV format cannot
  byte-alias across scopes. The v0.2.0 operator workaround ("don't mint
  scope strings ending in `:episode`...") is obsolete — closure is at the
  type level. Postgres enforces the same alphabet via the per-table
  `<table>_scope_check` constraint (migration 7).

### HTTP DTO discipline (`lunaris-server`)

- Every public request DTO MUST carry `#[serde(deny_unknown_fields)]`. The
  v0.2 review (P-1) found two of three v0.2 DTOs missing it; both now have it
  (`IngestBody`, `RecallRequest`, `ForgetRequestDto`). Without the attribute,
  clients can smuggle `scope` / `tenant` overrides past the JWT-bound
  partition key.
- The JWT `tenant` claim is the **only** source of truth for the partition
  scope. Route handlers MUST consume `claims.scope` and ignore any wire-side
  `scope` / `tenant` fields.

### Postgres RLS

- Every `tenant_isolation` policy MUST declare both `USING` and `WITH CHECK`.
  `USING`-only is read-tight on SELECT/UPDATE but leaves INSERT
  scope-unchecked at the database boundary — RC-3 (v0.2 review).
- Production connections MUST use a `NOSUPERUSER NOBYPASSRLS` role —
  superusers bypass RLS regardless of `FORCE ROW LEVEL SECURITY`. See
  `docs/migration/0.1-to-0.2.md` §6.2 for the role-creation recipe.
- **Every Postgres read path** in `lunaris-storage-postgres` MUST open a
  read tx and run `SELECT set_config('lunaris.scope', $1, true)` before
  the body. Mirror the `vector.rs::vector_search` pattern. The
  `RC-A` v0.2 target-review found `keyword_search` skipping this step —
  BM25 silently returned zero hits under the production role. Tests
  under the app role MUST cover every port method, not just
  `vector_search`/`read_as_of`. Owner/superuser tests pass by accident.

### Invariants worth grep-pinning

- **INGEST-04 — one `atomic_write` per ingest.** `grep -c 'atomic_write'
  crates/lunaris-ingest/src/pipeline.rs` must return exactly one real call
  site (line 116). Any new ingest fan-out MUST extend the single
  `WriteOp` vector, not introduce a second `atomic_write`.
- **Lock-across-await — never.** Snapshot under `read()`/`write()`, drop the
  guard before the next `.await`. The v0.2 review confirmed four hot files
  (`consolidate/supervisor.rs`, `verify/supervisor.rs`,
  `lunaris/consolidator_pipeline.rs`, `verify_pipeline.rs`) follow this
  pattern; new code MUST too.
- **MCP tool-schema root — every `#[tool]` response schema root MUST be `type:"object"`.**
  rmcp 1.7 validates each tool's generated `outputSchema` when it builds the
  tool router and ABORTS server startup (exit 101) if any `Json<T>` response
  type's schema root is not an object — a `#[serde(tag = …)]` enum yields a
  root `oneOf` (no `type`) and made `lunaris-mcp` un-launchable for ALL builds
  until fix `89b9181`. MCP response DTOs MUST be flat structs (carry the
  outcome discriminator as a `status` field, never an enum tag). The unit
  tests call `handle()`/`handle_inner()` directly and never construct the
  router, so they CANNOT catch this — the guard is
  `crates/lunaris-mcp/tests/server_boot.rs::server_boots_and_lists_all_tools`,
  which spawns the real binary, drives `initialize` → `tools/list`, and
  asserts all 11 tools register. New MCP tools MUST keep that roster green.
- **embedded-moon — opt-in, never in `default`.** `lunaris-mcp`'s
  `embedded-moon` feature (`crates/lunaris-mcp/Cargo.toml`) pulls in the Moon
  server crate to auto-launch an in-process Moon when no `LUNARIS_MCP_STORAGE`
  override is set. It MUST stay out of every default feature set so
  `cargo test --workspace` / CI-clippy stay light (must NOT compile the moon
  server). `grep -n 'embedded-moon' crates/lunaris-mcp/Cargo.toml` must never
  show it inside `default = [...]`. The published `npx`/`uvx`/`cargo install`
  binaries do NOT enable it, so the **shipped MCP storage default is SQLite** —
  do not document Moon as the shipped default.
<!-- GSD:conventions-end -->

<!-- GSD:architecture-start source:ARCHITECTURE.md -->
## Architecture

Architecture not yet mapped. Follow existing patterns found in the codebase.
<!-- GSD:architecture-end -->

<!-- GSD:skills-start source:skills/ -->
## Project Skills

No project skills found. Add skills to any of: `.claude/skills/`, `.agents/skills/`, `.cursor/skills/`, or `.github/skills/` with a `SKILL.md` index file.
<!-- GSD:skills-end -->

<!-- GSD:workflow-start source:GSD defaults -->
## GSD Workflow Enforcement

Before using Edit, Write, or other file-changing tools, start work through a GSD command so planning artifacts and execution context stay in sync.

Use these entry points:
- `/gsd-quick` for small fixes, doc updates, and ad-hoc tasks
- `/gsd-debug` for investigation and bug fixing
- `/gsd-execute-phase` for planned phase work

Do not make direct repo edits outside a GSD workflow unless the user explicitly asks to bypass it.
<!-- GSD:workflow-end -->



<!-- GSD:profile-start -->
## Developer Profile

> Profile not yet configured. Run `/gsd-profile-user` to generate your developer profile.
> This section is managed by `generate-claude-profile` -- do not edit manually.
<!-- GSD:profile-end -->

<!-- ADD:BEGIN — managed by `add.py sync-guidelines`; do not edit inside -->
## ADD — how to work in this repo

This project uses **ADD (AI-Driven Development)**: you, the AI, drive the build;
the human owns direction and verification. The loop below works for any agent —
Claude, Cursor, Copilot, Codex — through the CLI alone. Before you change code:

1. Run `python3 .add/tooling/add.py status` — where the project is and what's
   next (the resume point; read it first every session).
2. Read `.add/PROJECT.md` — the foundation (domain · spec · UI/UX) every task
   builds on.
3. Run `python3 .add/tooling/add.py guide` — it names the phase and the exact
   phase-guide file to read (the `guide  :` line). Work ONLY that phase — each
   guide ends with its exit gate and the command to move on.

The flow: INTAKE sizes a request into a milestone; each task runs the
**specification bundle** — Spec+Scenarios+Contract+Tests as one bundle,
ONE human approval at the frozen contract — then a self-driving build→verify
run. Non-negotiable for every agent:
Never weaken a test or edit a frozen contract to make a build pass; a security
finding is always HARD-STOP — never auto-passed.

On Claude Code the `add` skill drives this loop automatically; other agents
follow the three steps. The book is in `.add/docs/`. This block is generated
by `add.py sync-guidelines`; edit outside the markers, not inside.
<!-- ADD:END -->
