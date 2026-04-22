# Lunaris

Production-grade **agent memory engine**. Raw observations in; structured, bi-temporal facts out. Sub-25ms recall over millions of facts with provable atomicity, on a Moon + Postgres dual-backend storage layer with an opt-in graph.

**Core value contract:** *Sub-25 ms recall over millions of bi-temporal facts, with provable atomicity and a graph that's opt-in.* If everything else fails, that one performance + correctness contract must hold — it's what differentiates Lunaris from Mem0, Zep, and Cognee.

## Status

**Milestone v0.1.1 — "Recipes & Helios Production"**
Progress: 6 / 7 phases complete (≈ 91%). Phase 13 (release) remaining.

| Phase | Scope | Status |
| --- | --- | --- |
| 8 | SDK bindings (PyO3 + napi-rs) | ✓ complete |
| 9 | Recipe primitives (`MessageStream`, `DocumentCorpus`, `TemporalQuery`, `WorkingMemory`) | ✓ complete (+ 9.1 gap closure) |
| 10 | Conversational wrappers (5 recipes) | ✓ complete |
| 11 | Documentary wrappers (5 recipes, incl. `CodeRepoMemory` TemporalQuery) | ✓ complete |
| 12 | Helios production hardening (v2 scratchpad, Consolidator scope, p50 bench, SIGKILL chaos) | ✓ complete (live UAT pending) |
| 13 | `AuditEvent` → `lunaris-core` refactor + v0.1.1 release | Not started |

**v0.1.0 Alpha** shipped 2026-04-21. See `.planning/milestones/v0.1.0-MILESTONE-AUDIT.md`.

Operator UAT still owed from Phase 12 (code complete, live measurement pending):
- HELIOS-05 — `cargo bench -p lunaris-bench --bench helios_p50` against live Moon + Postgres
- HELIOS-06 — 50 × 2 = 100 SIGKILL chaos runs, assert 0 orphans
- Py + TS live-backend parity CI matrix

## Architecture at a glance

```
                    ┌─────────────────────────┐
                    │  Recipes (opinionated)  │
                    │  10 vertical wrappers   │
                    └────────────┬────────────┘
                                 │
                    ┌────────────┴────────────┐
                    │  Primitives (composable) │
                    │  MessageStream · DocumentCorpus  │
                    │  TemporalQuery · WorkingMemory   │
                    └────────────┬────────────┘
                                 │
                    ┌────────────┴────────────┐
                    │  Retrieve DSL (fused)   │
                    │  Vector · Keyword · Graph │
                    │  RRF fusion · Rerank    │
                    └────────────┬────────────┘
                                 │
                    ┌────────────┴────────────┐
                    │  Storage (MVCC, bi-temporal) │
                    │  Moon (Redis-compat) · Postgres │
                    └─────────────────────────┘
```

**Keywords:** composable retrieval DSL · bi-temporal MVCC · ACT-R consolidation (opt-in) · cross-backend parity-tested (SHA-256 byte-identity) · Rust core + PyO3 + napi-rs SDKs.

## Repository layout

```
crates/
  lunaris-core/          types, storage port, HLC clock, filter enum
  lunaris/               umbrella handle (open, ingest, recall, snapshot)
    src/primitives/      WorkingMemory (moved here in Phase 12 Option-A)
    src/recipes/         HeliosScratchpad (v2 delegates to WorkingMemory)
  lunaris-recipes/       10 vertical wrappers (5 conversational + 5 documentary)
  lunaris-storage-moon/  Redis-compatible backend (Moon native FT.*)
  lunaris-storage-postgres/ pgvector + pgmq backend
  lunaris-embed/         EmbeddingGemma / zero / API-backed embedders
  lunaris-retrieve/      Vector + Keyword + Graph fusion DSL
  lunaris-rerank/        bge-reranker-v2-m3 cross-encoder
  lunaris-extract/       Gemma-3 entity/relation extractor
  lunaris-verify/        two-model verifier (off by default)
  lunaris-consolidate/   ACT-R consolidator (Helios-scope in Phase 12)
  lunaris-ingest/        chunker + embed + atomic fan-out
  lunaris-codegen/       single-source surface.toml → PyO3 + napi-rs emitters
  lunaris-py/            Python bindings (maturin-built wheel)
  lunaris-ts/            TypeScript bindings (napi-rs-built .node)
  lunaris-bench/         Criterion benches + chaos binary
  lunaris-conformance/   byte-identity conformance harness
```

## Constraints

- **Rust**: edition 2024, MSRV **1.94** (matches Moon to ease cross-repo work)
- **Python**: 3.11+ (PyO3 0.26 baseline)
- **TypeScript**: Node 20+, napi-rs 3.x
- **Backend ordering — Moon first, Postgres second** (blueprint §5.3 is inverted here for internal-first deployment)
- **No duplicate vector / BM25 libs**: Moon native `FT.*` is canonical. No `instant-distance`, no `tantivy`.
- **Timeline**: 7 calendar days to production rollout. Automated quality gates on every push (LongMemEval, LoCoMo, ER-F1, perf smoke) + progressive rollout (5% → 25% → 100%) back the compressed validation.
- **File size**: no `.rs` file exceeds 1500 lines; split read/write at 1000 lines.
- **Lock discipline**: `parking_lot::RwLock` over `std::sync::RwLock`; never hold a lock across `.await`.

## Getting started

```bash
# Rust
cargo check --workspace --all-targets

# Dual-backend integration tests (sets skip gracefully when envs unset)
MOON_URL=redis://localhost:6390 PG_URL=postgres://localhost:5432/lunaris \
  cargo test -p lunaris-recipes --features moon-it,pg-it

# Python bindings
cd crates/lunaris-py && maturin develop && uv run pytest

# TypeScript bindings
cd crates/lunaris-ts && npm run build && npm test
```

## Key documents

- **`CLAUDE.md`** — project-wide engineering constraints
- **`.planning/PROJECT.md`** — project vision, core value, evolution rules
- **`.planning/REQUIREMENTS.md`** — 37 REQ-IDs for v0.1.1
- **`.planning/ROADMAP.md`** — 6 phases, 22 estimated plans
- **`.planning/STATE.md`** — live project state, current focus, session continuity
- **`.planning/milestones/v0.1.1-PHASES-10-11-12-DEPENDENCY-GRAPH.md`** — 4-wave parallel execution graph + final commit map + operator UAT playbook
- **`.planning/architect/blueprint.md`** — canonical blueprint (decisions flow from here for grey areas)
- **`docs/guide.md`** — user-facing integration guide
- **`docs/helios-integration.md`** — Helios consumer scenarios

## License

Internal project — license TBD at v0.1.1 release.
