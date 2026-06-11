# GLOSSARY  (one name per concept — used everywhere: specs, contracts, code)

<!-- Lunaris domain terms (evidence-grounded: lunaris-core, root CLAUDE.md, docs/ARCHITECTURE.md) -->
Episode: raw observation envelope an agent ingests (message, document, tool result); source of all derived primitives.
Chunk: embedded retrieval unit derived from an Episode (adaptive hierarchical chunking, v0.6).
Entity / Relation / Fact: graph primitives extracted from Episodes; Fact carries bi-temporal validity.
Community: graph-cluster summary primitive (RAPTOR/GraphRAG layer).
Scope: validated multi-agent partition key, alphabet `[A-Za-z0-9_\-.]{1,128}`; prefixes every KV key `lunaris:{scope}:{kind}:{ulid}` and every FT index / graph / MQ topic name.
WriteOp: one operation in the single atomic_write batch (KvPut, KvDelete, VectorUpsert, GraphNode, GraphEdge).
Hit: scored recall result (id, score, optional content preview / episode_id).
Moon: internal Redis-compatible substrate (vendor/moon submodule); canonical vector (FT.*), BM25, graph (Cypher), MQ, TXN, TEMPORAL implementation.
moondb: the Moon Rust SDK crate, path-dep at vendor/moon/sdk/rust, aliased `moon` in the workspace.
atomic_write: the one-per-ingest transactional write — Moon TXN.BEGIN→ops→TXN.COMMIT→TEMPORAL.SNAPSHOT_AT (INGEST-04 invariant).
retrieval ladder: MCP progressive disclosure — scratchpad exact tier → recall k=5 preview → widen k/source_prefix/as_of.
native RRF: Moon-side 3-way hybrid fusion (`FT.SEARCH ... hybrid_search`, weights [bm25, dense, sparse]) replacing client-side RRF when capabilities allow.
FT.NAVIGATE: Moon command fusing KNN seeds with graph expansion server-side (hop penalty, optional DECAY λ recency re-rank) — target of the moon-v030-exploit milestone.
decay (λ): Moon temporal-decay traversal scoring — per-edge cost `|weight| + λ·w·age_seconds` in shortestPath / FT.NAVIGATE re-rank; recency bias for agent memory.
SQ8: Moon per-vector affine 8-bit scalar quantization (`FT.CREATE ... QUANTIZATION SQ8`), working as of Moon v0.3.0; TQ4 is the 4-bit alternative that shines at 768d+.
HOTKEYS: Moon command returning top sampled keys (SpaceSaving sketch, 1-in-64 sampling) — observability surface.
hash tag: `{tag}` braces in a Moon key routing it to one shard; candidate mechanism for multi-shard TXN compliance (`lunaris:{scope}:...`).

# ADD method vocabulary (domain-standard names; bridges to legacy terms)
GOAL: the one durable outcome a project (and each milestone) runs toward — the loop's orientation anchor, declared as the lowercase `goal:` line in PROJECT.md / MILESTONE.md and surfaced by status/guide every session; distinct from a task's §1 Must (a single required behavior, not the whole-project outcome).
deep verify: the deepened Verify evidence (v20) required beyond passing tests — for a task that produced code, that every new symbol is referenced (wiring) and no new dead/unused code exists; for prose/non-code, a recorded no-skim semantic read; which path applies is resolver-judged and the engine never classifies (a rubric, not add.py).
onboarding: the install -> first-milestone path (formerly "on-ramp").
primary flow: the solid forward path of the flow diagram — a phase starts only when its input exists (formerly "forward spine").
cross-cutting concern: a concern running through every step rather than being one step — security, testing, observability, cost (formerly "spine / continuous concern").
working state: everything an agent loads each session — skill router, active phase, PROJECT/MILESTONE/TASK, state.json (formerly "state surface").
audit trail: the reference record read by people, never auto-loaded into agent context (formerly "story surface").
method rationale: the why behind every rule — the AIDD book, loaded on demand, never duplicated (formerly "trust layer").
failing-first suite: the test suite written before code, confirmed red for the right reason — a missing implementation (formerly "red safety net").
non-functional review: the deliberate post-evidence check of what tests rarely catch — concurrency, security, architecture (formerly "blind-spot checks").
change scope: the files a locked run may and may not touch (formerly "touch-boundary"; the <touch_boundary> XML prompt tag keeps its name).
automated quality gate: the evidence-based Verify resolver under autonomy auto — may auto-PASS on complete evidence; security always escalates (formerly "evidence auto-gate").
autonomy level: the per-task Verify resolver setting — auto (default) or conservative; declared in the TASK.md header, human-reviewed at the freeze (formerly "autonomy dial").
living documentation: the durable project artifacts — conventions, glossary, frozen contracts — that outlive any particular code (formerly "survivor layer").
scope level: the granularity a decision lives at — intake level (request -> versioned scope), milestone level, setup/foundation level, task level (formerly "altitude").
baseline approval: the one human gate that freezes the AI-drafted foundation, first scope, and first contract together — runs as `add.py lock` (formerly "the lock-down").
lesson learned: a single learning a loop produces, tagged by the competency it improves — the `- [DDD · open]` grammar and deltas.md/`add.py deltas` machine names stay (formerly "competency delta").
lowest-confidence flag: the AI's ranked declaration of the 1–2 points most likely to be wrong in what a human is asked to approve — each with why + cost-if-wrong; the ⚠ glyph keeps its name as the machine marker (formerly "least-sure flag").
decision point: a stop for human judgment — the contract-freeze approval, an escalated verify gate, intake confirmation, milestone close; the machine names seam (--json owner enum, decide key) and seam-audit (CI job) keep their names (formerly "seam").
retrospective consolidation: gathering confirmed lessons learned at milestone close and writing them append-only into the versioned foundation — human-confirmed, never self-approved; the machine names fold.md, the folded status, and add.py deltas keep their names (formerly "the fold / fold ritual").
specification bundle: a task's spec, scenarios, contract, and failing tests drafted as one piece and approved by a person once at the contract freeze (formerly "the one-approval front").
