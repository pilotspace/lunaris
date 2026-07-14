# TASK: lunaris-mcp opens with the interactive (small-budget) embedder

slug: mcp-embed-budget · created: 2026-07-14 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it, or run `add.py autonomy set`. -->
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower the
     autonomy level to `manual` or `conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-mcp/src/state.rs:195` — bootstrap calls `Lunaris::open(&storage_url)`, i.e. the
  GENERAL embedder path (`resolve_embedder(embed_max_batch_tokens())` = 4096-token window). MCP is a
  long-lived interactive daemon like contextd, so it inherits the same worst-case llama.cpp compute-
  buffer reservation (~2.3 GB on a heavy embed batch) that contextd just fixed (commit 28cfa73).
- `lunaris::resolve_default_embedder` — the interactive/small-budget entry (1024-token default via
  `LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS`), shared once; already consumed by contextd.
- `Lunaris::open_with_embedder(url, embedder)` (handle.rs:328) — opens with a BYO embedder.
- `crates/lunaris-ingest/src/pipeline.rs:92` — `DEFAULT_TARGET_TOKENS = 500`: the embedder only ever
  sees CHUNKS (~500 tok) and QUERIES (short), never whole documents — so a 1024 budget has 2x
  headroom and NEVER truncates MCP's ingested content (the doc is chunked before embedding).
Context (working folder): lunaris-mcp (pid 12915) was 558 MB in the screenshot — not spiking yet,
  but the same resolve_embedder(4096) path means a heavy `memory.ingest` would balloon it like
  contextd. Applying the interactive budget pre-empts that. BootstrapError has From<LunarisError>
  (state.rs:32) so `resolve_default_embedder().await?` propagates cleanly.
Honors (patterns / conventions): mirrors contextd's `open_with_embedder(resolve_default_embedder())`
  seam; env-tunable; no new dep; the embedder-health probe (line 213) still runs on the result.
Anchors the contract cites: `Lunaris::open_with_embedder`, `resolve_default_embedder`, MCP bootstrap.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: lunaris-mcp opens its Lunaris handle with the shared interactive (small-budget) embedder,
so its llama.cpp context reserves ~1.1 GB not the ~2.3 GB worst-case batch buffer.
Framings weighed:
  - open_with_embedder(resolve_default_embedder()) (CHOSEN) — reuses contextd's proven seam +
    already-tested budget logic; env-tunable; one line.
  - set LUNARIS_EMBED_MAX_BATCH_TOKENS in MCP's launch env (rejected) — relies on the operator/
    installer setting it; not self-contained; the shipped default would still be 4096.
Must:
<must>
  - MCP bootstrap opens via `Lunaris::open_with_embedder(url, resolve_default_embedder().await?)`
    (the 1024-token interactive default), NOT `Lunaris::open` (4096).
  - The embedder-health probe still runs against the resulting handle (no bootstrap regression).
  - The MCP server still launches and lists all tools (server_boot guard stays green).
Reject:
<reject>
  - resolve_default_embedder error -> propagates as BootstrapError::LunarisOpen (From<LunarisError>),
    exactly as Lunaris::open's error did (no new failure mode)
</reject>
After:
<after>
  - Live MCP footprint on a heavy ingest is bounded (~1.1 GB class) not ~2.3 GB.
  - server_boot guard green; recall/ingest still correct (embedder sees <=1024-tok chunks/queries).
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ MCP never embeds an input longer than 1024 tokens — lowest confidence because a caller could
    `memory.ingest` a huge single document. If wrong: truncation. Cost: NONE in practice — ingest
    chunks to DEFAULT_TARGET_TOKENS=500 before embedding, so the embedder only sees ~500-tok chunks
    + short queries; 1024 has 2x headroom. Env-tunable up if a future chunker raises the target.
  - [x] budget logic already tested — parse_batch_tokens + the 1024/4096 default test (contextd-embed-budget).
  - [x] BootstrapError has From<LunarisError> — confirmed (state.rs:32).
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: MCP boots with the interactive embedder
  Given the MCP server binary and a storage backend
  When it bootstraps
  Then it opens via open_with_embedder(resolve_default_embedder) and lists all tools

Scenario: resolve failure is not a new failure mode
  Given resolve_default_embedder returns Err
  When MCP bootstraps
  Then it surfaces BootstrapError::LunarisOpen (same as Lunaris::open did), no panic
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
STATE.RS (lunaris-mcp) bootstrap
  - let embedder = lunaris::resolve_default_embedder().await?;     // 1024 interactive default
    let lunaris = Lunaris::open_with_embedder(&storage_url, embedder).await?;
  (replaces `let lunaris = Lunaris::open(&storage_url).await?;`)
  probe_embedder_health(&lunaris.embedder()) still runs unless skip_probe.

ENV (inherited, no new var): LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS tunes it (shared with contextd,
  both interactive daemons).
UNCHANGED: consolidator install; embedder-health probe; storage resolution; the tool roster.
```

Status: FROZEN @ v1 — approved by AI auto-gate (Tin reviews at PR).
Least-sure flag surfaced at freeze: [spec] sharing LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS between
  contextd and MCP (the name says "CONTEXT") — cost: cosmetic; both are interactive daemons wanting
  the same small budget; a rename is deferrable churn. No new failure mode; server_boot guards launch.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: the existing MCP boot guard + the already-green budget unit tests.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - server_boots_and_lists_all_tools (existing guard, crates/lunaris-mcp/tests/server_boot.rs):
    spawns the real binary through the NEW bootstrap path, drives initialize→tools/list, asserts all
    tools register. This IS the discriminating test that the interactive-embedder open path still
    launches. (No new unit test: the budget difference is unobservable from the handle without real
    GGUF weights — CI resolves NoopEmbedder either way — so the budget value itself is covered by
    contextd-embed-budget's parse_batch_tokens + 1024/4096 default tests, and the MCP-specific
    footprint is a live proof, not a CI unit test. Honest gap, documented.)
</test_plan>

Tests live in: `crates/lunaris-mcp/tests/server_boot.rs` (existing guard) — must stay green.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-mcp/src/state.rs`
Strategy (ordered batches): 1. swap `Lunaris::open` → `open_with_embedder(resolve_default_embedder())`.
  2. build + clippy. 3. server_boot guard green. (No new red test — this is a wiring change over
  already-tested budget logic; the server_boot guard is the regression net.)
Safety rule (feature-specific): the resolve error must still map to BootstrapError (From<LunarisError>);
  the health probe still runs; no new failure path.
Code lives in: `crates/lunaris-mcp/src/state.rs`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — server_boot guard 1/1 (real binary launches through the new bootstrap path, lists all tools); clippy `-p lunaris-mcp` clean
- [x] coverage did not decrease — no test removed; the boot guard exercises the new path
- [x] no test or contract was altered during build — §3 FROZEN @ v1 as implemented
- [x] the green was EARNED — server_boot spawns the REAL binary and drives initialize→tools/list; it would fail if the new open path panicked or dropped a tool. Not a stub.
- [x] concurrency / timing safe — one extra `.await` (resolve_default_embedder) before open, both already async; no lock; the embedder is loaded once at bootstrap as before
- [x] no exposed secrets / injection / unexpected deps — internal wiring only; no new crate (lunaris already a dep)
- [x] layering & dependencies follow CONVENTIONS.md — uses the public `lunaris::resolve_default_embedder` + `open_with_embedder` surface (same as contextd)
- [ ] a person reviewed and approved the change — auto-gate (non-security); Tin reviews at PR #55 merge

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
- [x] MCP bootstrap opens via the interactive embedder — code at state.rs:195 (open_with_embedder(resolve_default_embedder())); grep shows no bare `Lunaris::open(` left in the bootstrap
- [x] MCP still launches + lists all tools — server_boot guard green (the CLAUDE.md un-launchable-regression net)
- [x] budget default carries over — `context_embed_max_batch_tokens()==1024` proven in contextd-embed-budget; MCP now routes through it

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `resolve_default_embedder` + `open_with_embedder` both referenced at state.rs:195; the resulting handle feeds the existing consolidator install + health probe. clippy clean.
- [x] DEAD-CODE (code) — no orphan; the old `Lunaris::open` call was replaced, not left dangling.
- [x] SEMANTIC — n/a (code task).

### GATE RECORD
Outcome: PASS
Auto-resolved under `autonomy: auto` — server_boot guard green through the new path, clippy clean,
budget logic already unit-tested. Non-security. Live MCP footprint proof deferred (same mechanism as
contextd's proven 2458→1126 MB; MCP wasn't spiking). Owner reviews at PR #55 merge.
Reviewed by: AI auto-gate (Tin reviews at PR) · date: 2026-07-14

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>

### Spec delta
- [SPEC · open] the embedder budget floor should be DERIVED from the chunker's target
  (DEFAULT_TARGET_TOKENS + overlap + margin) so the two independent literals can't drift into
  truncation (evidence: today budget=1024 and chunk target=500 are unlinked constants in different
  crates; a chunk-target bump would silently truncate).
- [SPEC · open] embedder/reranker high-perf non-blocking architecture: separate latency classes
  (query vs ingest workers), micro-batch coalescing, bounded context pool, drop the spawn_blocking
  hop for a oneshot reply, semaphore+shed-to-BM25 under load (evidence: 2026-07-14 design Q; single
  EncodeWorker serializes + head-of-line-blocks recall behind ingest batches).
- [SPEC · open] consider renaming LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS →
  LUNARIS_INTERACTIVE_EMBED_MAX_BATCH_TOKENS now that MCP shares it (cosmetic).
Forward changes for the next loop — each re-enters at Specify as the next task. One line
each, tagged `[SPEC · open|seeded|dropped]`, with evidence (e.g. `[SPEC · open] rate-limit
the retry path (evidence: prod herd spikes)`). See the `add` skill's `deltas.md`.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
