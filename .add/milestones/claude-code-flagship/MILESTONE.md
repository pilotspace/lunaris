# MILESTONE: Claude Code Flagship Memory

goal: A Claude Code session recalls prior-session memory at flagship quality — the graph+KV hybrid path that scored J=96% serving the production hook, filter-correct on every Moon retrieval surface, storage-lean at rest, installed in two commands
rationale: intake bucket `new-major` — Tin Dang 2026-07-14 delegated scope + execution ("act as project owner … you decide implement Lunaris to ship it in limit timebox now"). New product theme: Lunaris as the top-tier memory engine for Claude Code (coding agent) exploiting the full Moon stack. Relationship: *extends* moon-v030-exploit (graph+KV hybrid, J=96% bench config) and hook-session-scratchpad (the hook/MCP surface); *continues* the 2026-06-11 Moon-only direction; *adopts* the loose open task ft-navigate-filter-gap (its blocker moon-hybrid-filter-bypass is done). Grounded 2026-07-14: hook context inject serves ONLY `recall_vector_hot_path`/`recall_keyword_hot_path` (crates/lunaris-hook/src/context.rs:427-509) — the J=96% graph+KV config never reaches Claude Code; MQ is already load-bearing (ingest→consolidate/verify pub/sub) so no MQ task; the KV embedding double-store fix (project_moon_embedding_doc_redundancy) is still queued.
stage: production · status: active · created: 2026-07-14

> SDD living doc for this milestone. Keep it THIN: breadth, shared decisions, and
> exit criteria only — per-task detail lives in each `.add/tasks/<slug>/TASK.md`,
> written just-in-time. Update this doc whenever a task reveals a milestone gap.

## Scope
In:  graph+KV hybrid recall on the production hook path (prompt + tool phases, latency-budgeted with graceful degrade to today's vector path) · filtered-Navigate correctness on Moon (the ft-navigate-filter-gap silent-wrong-results hole; interim client-side guard is a shippable slice per its §1 framings) · at-rest KV slimming (drop the 768-d JSON embedding floats from chunk/entity/fact/community hydration docs — binary vec already lives in the FT index) · two-command Claude Code turnkey wiring (mcp + hook recipe verified end-to-end + a "Lunaris for Claude Code" docs page)
Out: the moon-only backend deletion sweep (own milestone) · flipping the shipped MCP storage default to Moon (turnkey documents Moon opt-in; SQLite default stays — CLAUDE.md invariant holds) · Moon-server-side FT.NAVIGATE FILTER machinery beyond what the timebox allows (interim guard acceptable; full fix recorded as follow-on) · SPLADE / multimodal · live Mem0-comparable UAT numbers (mem0-parity exit criterion #3 stays HUMAN-UAT) · Codex-fork parity beyond existing HOOK-07 behavior

## Shared decisions & glossary deltas   (living — every task must honor these)
- Design for failure: hybrid recall on the hook path carries a hard timeout + fallback to the current vector hot path — a slow or down graph engine must NEVER stall a Claude Code session start or prompt.
- Built ≠ wired (foundation v3): every task's exit evidence drives the production surface (hook binary / MCP server / real Moon), never only the library seam.
- No second retrieval engine: the hook reuses `lunaris-retrieve` presets/DSL; zero hook-local ranking logic.
- Filter correctness precedes graph exposure: if the hook's hybrid recall passes any `.filter()`, it may only ship after (or gated behind) the ft-navigate-filter-gap guard — never expose the known leak to Claude Code.
- Scope alphabet `[A-Za-z0-9_\-.]{1,128}` for any session/repo-derived key component; JWT/hook-resolved scope stays the only partition source.
- Lock discipline + INGEST-04 unchanged (one `atomic_write`; KV slimming edits payload serialization, not write fan-out).

## Shared / risky contracts (freeze these first)
- hook hybrid-recall config surface (env toggles, latency budget, fallback semantics) -> owning task hook-recall-graph-hybrid
- at-rest KV doc shape sans embedding (hydrate/inspector read-model compatibility — the heterogeneous read model in PROJECT.md §Domain) -> owning task kv-embedding-slim
- Navigate-with-filter semantics (guard vs full fix; seeds AND BFS-expanded hits) -> owning task ft-navigate-filter-gap

## Tasks (breadth-first decomposition; detail lives in each TASK.md)
- [x] ft-navigate-filter-gap     depends-on: none — DONE 2026-07-14 (gate PASS, commit 0596986): Navigate guard + the discovered Moon-wide KNN filter silent-drop fixed (contract v1.1 rendering + post-filter); zero-filter path byte-unchanged
- [x] kv-embedding-slim          depends-on: none — CLOSED duplicate_goal 2026-07-14: already delivered by moon-v051-perf-exploit W3 (`#[serde(default, skip_serializing)]` on all four primitives; pin `c_embedding_skip_serialize.rs` re-verified green 11/11). The memory that seeded this task was stale.
- [x] hook-recall-graph-hybrid   depends-on: ft-navigate-filter-gap — DONE 2026-07-14 (gate PASS): four-leg fused root (v1.1 adds Keyword::bm25("facts")) serves lunaris-contextd's prompt/tool recall by default; hydrate_mixed makes facts injectable; timeout/error/empty degrade to the legacy path; live discriminator green on Moon
- [x] claude-code-turnkey        depends-on: hook-recall-graph-hybrid — DONE 2026-07-14 (gate PASS): `--verify` proof mode in setup-lunaris-agents.py (session-A capture → session-B cross-session inject through the installed hook commands, Moon autostart, stage-labeled fail-fast) + docs turnkey lead section

## Exit criteria (observable; map each to the task that delivers it)
- [x] A `.filter()`'d Navigate recall on live Moon never surfaces a filter-violating hit (BFS-expanded hits included), and zero-filter Navigate results are unchanged        (← ft-navigate-filter-gap; verifiers: crates/lunaris-retrieve/tests/navigate_filter_moon.rs (live discriminator) + navigate_fallback.rs routing pins + crates/lunaris-storage-moon/tests/vector_filter_moon.rs — green 2026-07-14, commit 0596986)
- [x] Chunk/entity/fact/community KV docs at rest carry no embedding float arrays; hydrate and inspector read paths stay green on Moon + embedded        (← kv-embedding-slim; satisfied by PRIOR work — moon-v051-perf-exploit W3, pin c_embedding_skip_serialize.rs green 2026-07-14; the "40x smaller KV" measurement is recorded in that milestone's ledger)
- [x] A real hook invocation (SessionStart/UserPromptSubmit) against live Moon injects a memory reachable ONLY via the graph path — proving the production hook is served by hybrid recall, within the latency budget, with fallback proven by a fault-injection test        (← hook-recall-graph-hybrid; verifier: crates/lunaris-hook/tests/context_hybrid_recall.rs::fact_surfaces_in_injected_context_moon drives the REAL lunaris-contextd binary — the process Claude Code's UserPromptSubmit hook consults — green on live Moon 2026-07-14; fault-injection = timeout_zero_degrades_to_legacy)
- [x] On a fresh checkout, ≤2 documented commands yield a Claude Code session whose transcript shows capture AND cross-session inject working        (← claude-code-turnkey; verifier: scripts/tests/test_turnkey_verify.py::test_two_command_turnkey_proves_capture_and_inject — runs the two documented commands verbatim, green on live Moon 2026-07-14; the "transcript" is --verify's stage output: capture in session verify-a, marker injected into session verify-b's additionalContext)

## Close — ship review   (AI fills when every task is done — the evidence behind the engine gate, read before the boxes are checked)
> Whole-milestone, cross-task review the AI fills in. It is the evidence behind the EXISTING engine
> gate (milestone-done / checking the Exit-criteria boxes) — NOT a new approval. Tool-agnostic.

### Ship by domain   (what changed, per bounded context)
- tooling : add.py `_SCOPE_EXCLUDE_DIRS` gains "target" (cargo build churn caused false scope_violations + 20-min walks); scripts/setup-lunaris-agents.py gains `--verify` + `--moon-autostart`
- skill   : untouched
- book    : docs/integration/claude-code.md gains the "Turnkey (two commands)" lead section

### Cross-task evidence   (one row per task)
- ft-navigate-filter-gap : gate=PASS · tests=15 green (2 live-Moon files + routing pins + 10 renderer/evaluator pins) · residue=Moon-side FT.NAVIGATE FILTER machinery + keyword-surface composite-filter probe recorded as follow-ons
- kv-embedding-slim : gate=PASS (closed duplicate_goal) · tests=pin c_embedding_skip_serialize.rs 11/11 re-verified · residue=none
- hook-recall-graph-hybrid : gate=PASS · tests=9 green (4 hydrate_mixed + 2 shape + 3 e2e incl. live discriminator) · residue=real-fact-embeddings, SourceOp facts weighting, embedded FTS5, contextd reranker (§7)
- claude-code-turnkey : gate=PASS · tests=4 green (incl. gated live two-command proof) · residue=curation scrubbed-nested-key bug + snippet-cap prompt-capture rendering (§7, production-relevant)

### Goal met?   (map the evidence back to this milestone's Exit criteria — read before the Exit-criteria boxes are checked)
- [x] each Exit criterion above is satisfied by a Cross-task evidence row (criterion 1 ← ft-navigate-filter-gap; 2 ← kv-embedding-slim/prior W3; 3 ← hook-recall-graph-hybrid; 4 ← claude-code-turnkey — verifiers cited inline per criterion)
- goal: a Claude Code session recalls prior-session memory at flagship quality — PROVEN by the chain: the J=96-family fused hybrid (now four-leg) serves the production lunaris-contextd path (live discriminator), filters are honored on every Moon vector KNN surface (filter-gap fix), KV is embedding-slim at rest (W3 pins), and a fresh checkout reaches a verified capture+cross-session-inject loop in two documented commands (turnkey proof).


## Release steps   (AI-DEFINED — fill the ordered steps to ship this milestone; engine records, human gate)
> The AI writes the release steps for THIS milestone here (hints, not engine commands). MERGE is one
> small step among them. These feed the release scope (release.md) when the cut is bundled.
- [ ] branch the three milestone commits (0596986 · a82879b · turnkey) off main as feat/claude-code-flagship and open a PR with the ship-review as description — ASK TIN before creating the PR (global git rule)
- [ ] owner merges with `gh pr merge --admin --rebase`, then `cargo fmt --all` on main (house rule)
- [ ] follow-on tasks from §7 residue (curation scrubbed-key bug is production-relevant) enter the next milestone intake
