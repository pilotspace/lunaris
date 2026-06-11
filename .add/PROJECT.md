# PROJECT — living documentation (cross-milestone context)

> The durable foundation that outlives every milestone and feeds context into each
> TDD⇄ADD loop. Read this FIRST in any session. Keep it lean — one screen, not a
> manual. Map to the AIDD diagram: Domain = DDD · Spec = SDD (living document) ·
> UI/UX = UDD. When a loop reveals a gap here, come back and update this file —
> that is the re-entrant arrow from the engine down to the foundation.

slug: Lunaris · stage: production · updated: 2026-06-11
goal: sub-25ms recall over millions of bi-temporal facts, with provable atomicity and an opt-in graph — the contract that differentiates Lunaris from Mem0/Zep/Cognee

---

## Domain (DDD) — the language and the boundaries
<!-- evidence-grounded: CLAUDE.md (root), crates/lunaris-core/src/ -->
- Core concepts: Episode · Chunk · Entity · Relation · Fact · Community · Scope (partition key) · WriteOp · Hit
- Bounded contexts / modules: lunaris-core (ports+types) · lunaris-ingest (extract→ONE atomic_write) · lunaris-retrieve (DSL: vector+BM25+graph→RRF) · lunaris-storage-{moon,postgres,embedded} (backends) · lunaris-{server,mcp} (surfaces) · lunaris-{py,ts} (SDKs)
- Invariants that must always hold:
  - INGEST-04: exactly ONE `atomic_write` call site in `crates/lunaris-ingest/src/pipeline.rs`
  - Keyspace `lunaris:{scope}:{kind}:{ulid}` minted ONLY by `lunaris_core::keyspace` helpers
  - Never hold a lock across `.await`; `parking_lot` only
  - JWT `tenant` claim is the only source of partition scope (wire `scope` fields ignored)
  - Every MCP `#[tool]` response schema root is `type:"object"` (flat structs, never tagged enums)

## Spec / Living Document (SDD) — what we are building, now
- Active milestone → `.add/milestones/moon-v030-exploit/MILESTONE.md` (see `add.py status`)
- Frozen contracts (living docs): `StoragePort`/`KeywordPort` (lunaris-core), HTTP DTOs (`deny_unknown_fields`), MCP tool roster (11 tools, `server_boot.rs` guard)
- Settled vs still open: Moon-first backend ordering settled · Moon pinned v0.3.0 (3e376a14) · open: SQ8-vs-TQ4 quantization at 768d, `{scope}` hash-tag multi-shard design

## Users (UDD) — UI/UX: design before code
- No UI — the surface is: Rust crate API, HTTP API (axum, lunaris-server), MCP stdio server (11 tools), Python/TS SDKs
- Primary users & jobs: internal agent platforms (Helios first) hiring Lunaris for durable agent memory: ingest observations → recall relevant context fast
- Core flows: ingest (extract→atomic write) · recall (retrieval DSL, progressive-disclosure ladder on MCP) · scratchpad (working memory) · consolidate (ACT-R)
- Design source of truth → docs/ARCHITECTURE.md + docs/book/ (mdBook)

## Key Decisions (append-only)
| date | decision | why | outcome |
|------|----------|-----|---------|
| 2026-04 | Moon-first, Postgres second | internal-first deployment, we own the substrate | blueprint §5.3 inverted; conformance suite covers both |
| 2026-05-14 | candle-native models default; fastembed/ONNX/Ollama-prod deleted | single ML runtime | v0.4 N-03 cutover |
| 2026-06-09 | MCP ships SQLite default; embedded-moon opt-in feature | `cargo test` stays light; npx/uvx binaries lean | PR #19 |
| 2026-06-11 | vendor/moon pinned v0.3.0 (3e376a14) | SQ8, background HNSW compaction, HOTKEYS, elastic budgets | quick task 260611-d5w; SDK pin parity held (moondb 0.2.0) |
| 2026-06-11 | ADD adopted for Moon-exploit wave; GSD continues owning .planning roadmap | spec/tests-first discipline for the 7-task wave | this foundation |
