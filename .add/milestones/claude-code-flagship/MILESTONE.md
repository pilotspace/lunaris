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
- [ ] ft-navigate-filter-gap     depends-on: none — (adopted pre-existing task) filtered Navigate recall on Moon honors the filter — interim guard: Some(filter) degrades to filtered fallback + client-side post-filter; zero-filter path byte-unchanged
- [ ] kv-embedding-slim          depends-on: none — stop serializing the 768-d embedding as JSON floats into chunk/entity/fact/community KV hydration docs (~5x at-rest reduction); hydrate + inspector parity proven on Moon + embedded
- [ ] hook-recall-graph-hybrid   depends-on: ft-navigate-filter-gap — ContextService prompt/tool recall upgrades from vector-only(+keyword fallback) to the graph+KV hybrid preset, latency-budgeted with graceful degrade; discriminating test = a graph-only-reachable memory surfaces in injected context
- [ ] claude-code-turnkey        depends-on: hook-recall-graph-hybrid — fresh machine → memory-enabled Claude Code (capture + inject) in ≤2 commands: verified `.mcp.json` + hooks recipe (SQLite default, Moon opt-in) + "Lunaris for Claude Code" docs page

## Exit criteria (observable; map each to the task that delivers it)
- [ ] A `.filter()`'d Navigate recall on live Moon never surfaces a filter-violating hit (BFS-expanded hits included), and zero-filter Navigate results are unchanged        (← ft-navigate-filter-gap)
- [ ] Chunk/entity/fact/community KV docs at rest carry no embedding float arrays; hydrate and inspector read paths stay green on Moon + embedded; the measured at-rest size reduction is recorded        (← kv-embedding-slim)
- [ ] A real hook invocation (SessionStart/UserPromptSubmit) against live Moon injects a memory reachable ONLY via the graph path — proving the production hook is served by hybrid recall, within the latency budget, with fallback proven by a fault-injection test        (← hook-recall-graph-hybrid)
- [ ] On a fresh checkout, ≤2 documented commands yield a Claude Code session whose transcript shows capture AND cross-session inject working        (← claude-code-turnkey)

## Close — ship review   (AI fills when every task is done — the evidence behind the engine gate, read before the boxes are checked)
> Whole-milestone, cross-task review the AI fills in. It is the evidence behind the EXISTING engine
> gate (milestone-done / checking the Exit-criteria boxes) — NOT a new approval. Tool-agnostic.

### Ship by domain   (what changed, per bounded context)
- tooling : <add.py / state.json / templates — what shipped, or "untouched">
- skill   : <SKILL.md / phases/* / guides — what shipped, or "untouched">
- book    : <docs/* — what shipped, or "untouched">

### Cross-task evidence   (one row per task)
- <slug> : gate=<PASS|RISK-ACCEPTED> · tests=<n green> · residue=<none|note>

### Goal met?   (map the evidence back to this milestone's Exit criteria — read before the Exit-criteria boxes are checked)
- [ ] each Exit criterion above is satisfied by a Cross-task evidence row or a Ship-by-domain change (cite which)
- goal: <restate the milestone goal — and the one evidence line that proves the ship meets it>

## Release steps   (AI-DEFINED — fill the ordered steps to ship this milestone; engine records, human gate)
> The AI writes the release steps for THIS milestone here (hints, not engine commands). MERGE is one
> small step among them. These feed the release scope (release.md) when the cut is bundled.
- [ ] <step — e.g. open a PR from the Close ship-review above; the human reviews + merges>
- [ ] <step — e.g. export the ship-review to a hand-off doc, e.g. `pandoc CLOSE.md -o close.docx`>
- [ ] <step — e.g. tag / publish / deploy  (human-run, per release.md)>
