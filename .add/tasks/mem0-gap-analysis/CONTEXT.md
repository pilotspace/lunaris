# Shared context — mem0-gap-analysis build (read this first)

You are one of several subagents gathering EVIDENCE for a competitive gap analysis of **Lunaris**
(a Rust agent-memory engine) vs **Mem0** (mem0.ai, open-source memory layer for AI agents). The
orchestrator will synthesize your findings into `docs/competitive/mem0-gap-analysis.md` and rank a
hardening backlog. Your job is evidence, not prose.

## Lunaris in one paragraph
Pure-Rust bi-temporal MVCC memory engine. Ingests observations → extracts primitives (Episode·Chunk·
Entity·Relation·Fact·Community) with a local LLM → ONE atomic write → recall via a composable DSL that
fuses vector + BM25 + graph with RRF. Storage = Moon (Redis-compatible, internal, primary) + Postgres +
SQLite. Surfaces: Rust crate, HTTP (axum, `lunaris-server`), MCP stdio (`lunaris-mcp`), Python/TS SDKs.
**Core value contract = sub-25ms recall over millions of bi-temporal facts, provable atomicity, opt-in graph.**

## The discipline you MUST follow
- **Built ≠ wired.** A primitive EXISTING in the tree is NOT parity. You must find the **production call
  site** (the ingest/recall/server path that actually invokes it). If a thing exists but nothing on the
  hot path calls it, say so explicitly — that is the single most valuable finding.
- **Cite anchors.** Every Lunaris claim → `path:symbol` (a real file + symbol). Every Mem0 claim → a
  fresh dated source (URL + access date 2026-06-14). No claims from memory.
- **Be adversarial about your own findings.** Prefer "I verified X by reading Y and it does/doesn't call Z".

## Use serena for code search (mcp__serena__*), not raw grep where a symbol tool fits.

## The 8 dimensions (the whole analysis is cut along these)
1. reliability / IO-failsafe   2. eval / accuracy   3. observability / ops   4. correctness / security
5. memory-update-intelligence (Mem0's ADD/UPDATE/DELETE/NOOP reconciliation)   6. multi-level-memory + categories
7. graph-quality   8. SDK / DX / integrations

## Verdict vocabulary (assign one per dimension you cover)
`ahead` · `at-parity` · `partial(built-not-wired)` · `gap-missing`

## Severity rubric (for your recommendation; orchestrator finalizes)
- P0 = risks correctness/atomicity/security/data-loss OR threatens the core value contract
  (sub-25ms · provable atomicity · opt-in graph). A Mem0 capability we LACK is P0 ONLY if its absence
  blocks the production story; otherwise P1.
- P1 = competitive disadvantage, no data-risk (most Mem0 parity gaps).  P2 = polish/deferrable.

## Return format (STRICT — return this, nothing else)
For each dimension you were assigned:
```
### <dimension>
verdict: <one of the 4>
mem0_capability: <what Mem0 does> — source: <URL> (2026-06-14)
lunaris_reality: <what Lunaris actually does on the PRODUCTION path>
evidence_anchor: <path:symbol that proves it; for at-parity, the production CALL SITE not just the def>
built_not_wired_note: <if applicable: what exists but is NOT called on the hot path>
recommended_severity: <P0|P1|P2> + one-line why
impact: <what closing this unlocks>  rough_effort: <S|M|L>
```
