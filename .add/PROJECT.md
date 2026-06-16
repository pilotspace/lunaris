# PROJECT — living documentation (cross-milestone context)

> The durable foundation that outlives every milestone and feeds context into each
> TDD⇄ADD loop. Read this FIRST in any session. Keep it lean — one screen, not a
> manual. Map to the AIDD diagram: Domain = DDD · Spec = SDD (living document) ·
> UI/UX = UDD. When a loop reveals a gap here, come back and update this file —
> that is the re-entrant arrow from the engine down to the foundation.

slug: Lunaris · stage: production · updated: 2026-06-16 · foundation-version: 3
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
- Moon substrate facts (folded 2026-06-11, moon-v030-exploit):
  - Moon TXN is SHARD-LOCAL and binds to the connection's ACCEPT shard — multi-shard `atomic_write` is impossible until Moon ships `TXN BEGIN PIN` (RFC: docs/design/scope-hashtag-txn-rfc.md); all deployment recipes stay `--shards 1`
  - Moon subcommand wire form is `TXN BEGIN`/`MQ <SUB>` (space-separated args); dotted spellings (`TXN.BEGIN`, `MQ.POP`) are server-unhandled — FT.*/GRAPH.* dotted intuition does not transfer
  - Production GraphEdge writes carry no explicit WEIGHT → graph decay is age-only re-ranking today
  - MQ `partition` is API-level metadata, not a wire concept, across all backends
- At-rest read model is HETEROGENEOUS (folded 2026-06-16, memory-inspector): "memory" has THREE on-disk shapes — episode/chunk/community KV = `lunaris_core` primitives; `fact:` KV = `lunaris_extract::Fact`; entity/relation are GRAPH nodes/edges, never KV. The `core` primitives are the aspirational model, not on-disk truth; `*_prefix` keyspace helpers exist for all six kinds but the happy path never KV-populates entity/relation — a helper's existence ≠ data behind it. Decode each kind at its real shape.

## Spec / Living Document (SDD) — what we are building, now
- Active milestone → `.add/milestones/moon-v030-exploit/MILESTONE.md` (see `add.py status`)
- Frozen contracts (living docs): `StoragePort`/`KeywordPort` (lunaris-core), HTTP DTOs (`deny_unknown_fields`), MCP tool roster (11 tools, `server_boot.rs` guard)
- Settled vs still open (folded 2026-06-11): Moon-first backend ordering settled · Moon pinned v0.3.0 (3e376a14) · SETTLED: SQ8 over TQ4 on synthetic 768-d (FP stays default; corpus-realism caveat documented) · SETTLED: the 25ms contract is judged on the retrieval-only noop-embedder pass (canonical decomposition when embed is in-loop) · CLOSED: client-side `{scope}` hash-tagging disproven — multi-shard gated on Moon `TXN BEGIN PIN` (RFC) · open: upstream Moon requests (TXN PIN · FT.INFO quantization field · Cypher `_key` registration · HOTKEYS windowed reset) · open: shared reverse key-parser in lunaris-core (key-shape knowledge lives in 3 places) · open: 10k×1k like-for-like strict-replay rerun before any cross-version latency delta is quoted · open: SDK-presence ≠ server-support — every new SDK helper adoption needs a one-shot live probe before contracting
- Spec-discipline (folded 2026-06-16): verify library-behavior claims before freezing them (axum `Query<T>` DOES 400 on unknown params — read-only query-DTO scope-safety comes from `claims.scope`, never `deny_unknown_fields`) · the ground phase earns its keep by catching codebase-contradicted premises before a contract freezes (a `provenance: Vec<Ulid>` field never serialized) · specify graph/Cypher contracts by observable reachability ("depth=1 excludes 2-hop"), NOT query syntax — freezing Cypher TEXT once froze a broken query that still passed the gate · make "a production-path integration test per scenario" a hard exit-gate item, not implicit · additive-default trait method (`Ok(())` default + one backend override) keeps a port extension's blast radius to one trait + one backend · trait-default methods that consume their input must carry the hazard in the doc · GHA `services:` blocks can't run locally-built images — pg-lunaris workflows use the integration.yml manual docker-run pattern, never `services:`.

## Users (UDD) — UI/UX: design before code
- No UI — the surface is: Rust crate API, HTTP API (axum, lunaris-server), MCP stdio server (11 tools), Python/TS SDKs
- Primary users & jobs: internal agent platforms (Helios first) hiring Lunaris for durable agent memory: ingest observations → recall relevant context fast
- Core flows: ingest (extract→atomic write) · recall (retrieval DSL, progressive-disclosure ladder on MCP) · scratchpad (working memory) · consolidate (ACT-R)
- Design source of truth → docs/ARCHITECTURE.md + docs/book/ (mdBook)
- Operator UX (folded 2026-06-11): cumulative-sketch gauges MUST carry "ranking, not rate" semantics in HELP text (operators alert without reading docs) · the Q4_K_M GGUF embedder is a legitimate high-throughput option, not a degraded mode (no visible recall cost at 3k-doc scale)

## Key Decisions (append-only)
| date | decision | why | outcome |
|------|----------|-----|---------|
| 2026-04 | Moon-first, Postgres second | internal-first deployment, we own the substrate | blueprint §5.3 inverted; conformance suite covers both |
| 2026-05-14 | candle-native models default; fastembed/ONNX/Ollama-prod deleted | single ML runtime | v0.4 N-03 cutover |
| 2026-06-09 | MCP ships SQLite default; embedded-moon opt-in feature | `cargo test` stays light; npx/uvx binaries lean | PR #19 |
| 2026-06-11 | vendor/moon pinned v0.3.0 (3e376a14) | SQ8, background HNSW compaction, HOTKEYS, elastic budgets | quick task 260611-d5w; SDK pin parity held (moondb 0.2.0) |
| 2026-06-11 | ADD adopted for Moon-exploit wave; GSD continues owning .planning roadmap | spec/tests-first discipline for the 7-task wave | this foundation |
| 2026-06-11 | SQ8 opt-in shipped, FP stays the FT default | tq4 collapses on synthetic 768-d (0.405); sq8 holds (0.995); real-corpus eval still owed | `?quant=` URL param; docs/migration/0.7-quantization.md |
| 2026-06-11 | 25ms contract judged on retrieval-only (noop-embed) pass | embed-in-loop dominates end-to-end (61.5ms vs 3.1ms); baseline was embed-out-of-loop | docs/benchmarks/v0.7-moon-v030-rerun.md |
| 2026-06-11 | Client-side hash-tagging rejected; multi-shard gated on Moon `TXN BEGIN PIN` | live probe: braced TXN rejected on 16/16 connections (accept-shard binding) | docs/design/scope-hashtag-txn-rfc.md + probe script |
| 2026-06-11 | Milestone moon-v030-exploit folded: 28 deltas → foundation v2 | retrospective consolidation per fold.md, confirmed by Tin Dang | conventions + domain facts appended; all deltas flipped `folded` |
| 2026-06-11 | **Moon-only direction**: Postgres AND SQLite backends deprecate-first (feature-gated off default), delete next minor; MCP default flips to Moon | Tin Dang: "only support Moon to maximize performance and features rich" — reverses the blueprint portability-proof constraint and the v0.5 SQLite-MCP-default decision | upcoming moon-only milestone owns the sweep (code gates, CI rows, docs, CLAUDE.md constraints, MCP packaging) |
| 2026-06-16 | Folded 32 open deltas → foundation v3 (memory-inspector close + accumulated multi-milestone) | retrospective consolidation per fold.md, confirmed by Tin Dang | §Domain heterogeneous-read-model bullet + §Spec spec-discipline line + CONVENTIONS v3 TDD/ADD sections; all 32 deltas flipped `folded` |
