<!-- GSD:project-start source:PROJECT.md -->
## Project

**Lunaris**

Lunaris is a production-grade **agent memory engine** built as a pure Rust framework with Python (PyO3) and TypeScript (NAPI) SDKs. It takes raw observations (messages, documents, tool results) from agents, extracts structured primitives using a small local LLM, and stores them in a bi-temporal MVCC store backed by **Moon** (our internal high-performance Redis-compatible substrate) with **Postgres** as a portability proof. Agents query it through a composable retrieval DSL that fuses semantic search, graph traversal, and BM25 keyword lookup.

The audience is internal agent platforms first (we own the substrate), with a public Rust crate / `pip install lunaris` / `npm i lunaris` story to follow. Helios is the first downstream consumer; LangGraph / CrewAI / Letta integration is a second-wave concern.

**Core Value:** **Sub-25ms recall over millions of bi-temporal facts, with provable atomicity and a graph that's opt-in.** If everything else fails, that one performance + correctness contract must hold — it's what differentiates Lunaris from Mem0, Zep, and Cognee.

### Constraints

- **Tech stack — Rust**: edition 2024, MSRV **1.94** (matches Moon to ease cross-repo work)
- **Tech stack — Python**: 3.11+ (PyO3 0.26 baseline)
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

Technology stack not yet documented. Will populate after codebase mapping or first phase.
<!-- GSD:stack-end -->

<!-- GSD:conventions-start source:CONVENTIONS.md -->
## Conventions

Conventions not yet established. Will populate as patterns emerge during development.
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
