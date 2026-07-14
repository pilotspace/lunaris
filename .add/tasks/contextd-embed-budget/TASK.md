# TASK: contextd embedder uses a small batch-token budget (1.1GB not 2.5GB)

slug: contextd-embed-budget · created: 2026-07-14 · stage: production
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
- `crates/lunaris/src/handle.rs:resolve_embedder` (1666) — builds `LlamaCppEmbedderOpts` with
  `..Default::default()` → `max_batch_tokens = 4096`. Callers: `Lunaris::open` (296, the general
  bench/MCP/ingest path) AND `resolve_default_embedder` (1780, contextd-only).
- `crates/lunaris/src/handle.rs:resolve_default_embedder` (1780) — contextd's entry (only caller:
  `lunaris-hook context.rs:201 shared_embedder`). Today just delegates to `resolve_embedder()`.
- `crates/lunaris-llamacpp/src/embedder.rs` (59, 131) — `max_batch_tokens: 4096` default; `budget =
  max_batch_tokens.max(16).min(n_ctx_train)`. `embed_blocking` (line ~160) `tokens.truncate(budget)`
  — so budget DOUBLES as the per-input token cap.
- `crates/lunaris-llamacpp/src/worker.rs:EncodeWorker::spawn` (66) — `.with_n_ctx(budget)
  .with_n_batch(budget).with_n_ubatch(budget).with_n_seq_max(64)`. The n_ubatch=budget compute
  buffer is the memory driver (proven).
Context (working folder): ROOT-CAUSE PROVEN 2026-07-14. contextd physical footprint = 2.5 GB (not
  the 453 MB `ps rss` — macOS compresses the idle working set; Activity Monitor shows phys_footprint).
  vmmap: 17×128 MB VM_ALLOCATE regions = 2.2 GB, all swapped. Reproduced: a fresh daemon jumps to
  2458 MB on the FIRST scope's captures (embed-promotion worker embeds a real batch), then FLAT
  (one-time worst-case llama compute-buffer reservation, NOT an unbounded leak, NOT per-scope).
  Empirical sweep (capture_probe.py, Moon 6381): max_batch_tokens 4096→2458 MB, 1024→1126 MB,
  512→809 MB. `n_seq_max` 64 vs 8 = IDENTICAL (not the lever). The lever is the batch-token budget.
  The recall path (query embed only, short) never triggers the worst case → stayed 344 MB.
Honors (patterns / conventions): design-for-failure (env-tunable, safe fallback default); the
  earlier `resolve_default_embedder` (shared-embedder fix) is contextd-only, so narrowing ITS budget
  leaves `Lunaris::open` (bench/ingest, wants big batches for long docs) untouched. No new dep.
Anchors the contract cites: `resolve_embedder`, `resolve_default_embedder`,
  `LlamaCppEmbedderOpts::max_batch_tokens`, env `LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: contextd's shared embedder uses a small batch-token budget (default 1024) so its llama.cpp
context reserves ~1.1 GB not ~2.5 GB; the general `Lunaris::open` embedder keeps 4096.
Framings weighed:
  - narrow ONLY resolve_default_embedder's budget, env-tunable (CHOSEN) — contextd-only, zero impact
    on bench/ingest/MCP; keeps the throughput budget where big batches matter.
  - shrink the global default 4096→1024 (rejected) — would truncate long docs in bench/ingest to
    1024 tokens, degrading embedding quality where it counts.
  - shrink n_seq_max (rejected) — proven to make NO memory difference (64 vs 8 identical).
Must:
<must>
  - `resolve_default_embedder` (contextd) resolves the llama.cpp embedder with `max_batch_tokens` =
    `context_embed_max_batch_tokens()` (env `LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS`, default 1024),
    NOT the 4096 default.
  - `Lunaris::open`'s `resolve_embedder` path keeps `max_batch_tokens` = 4096 (env
    `LUNARIS_EMBED_MAX_BATCH_TOKENS` may override, default 4096) — bench/ingest unchanged.
  - The budget helpers clamp to a sane floor (>=16, the embedder's own `.max(16)`), so a bogus env
    value cannot produce a zero/degenerate context.
Reject:
<reject>
  - unset / empty / non-numeric `LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS` -> default 1024 (fail-safe)
  - unset / empty / non-numeric `LUNARIS_EMBED_MAX_BATCH_TOKENS` -> default 4096 (unchanged behavior)
</reject>
After:
<after>
  - Live contextd capture workload footprint drops from ~2.5 GB to ~1.1 GB (capture_probe.py).
  - Recall + capture still return correct results (embeddings unchanged for inputs <=1024 tokens,
    which all real hook captures are).
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ 1024 tokens never truncates a real hook capture in a way that hurts recall — lowest confidence
    because a large Bash/Edit payload CAN exceed 1024 tokens (~4000 chars). If wrong: the tail of a
    long capture isn't embedded → slightly weaker recall for that episode. Cost: bounded — captures
    are scrubbed+chunked; the FIRST 1024 tokens (lead) carry the signal; env-tunable up if needed;
    and the raw text is still stored (only the vector is lead-weighted). Confirm: live recall still
    finds the shared-embedder episodes.
  - [x] the budget is the memory lever, not n_seq_max — confirmed empirically (sweep above).
  - [x] resolve_default_embedder is contextd-only — confirmed (single caller context.rs:201).
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: contextd budget default is 1024
  Given LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS is unset
  When context_embed_max_batch_tokens() is evaluated
  Then it returns 1024

Scenario: contextd budget honors the env override
  Given LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS=2048
  When context_embed_max_batch_tokens() is evaluated
  Then it returns 2048

Scenario: bogus contextd env falls back to default
  Given LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS="" or "abc"
  When context_embed_max_batch_tokens() is evaluated
  Then it returns 1024

Scenario: general budget default is 4096 (unchanged)
  Given LUNARIS_EMBED_MAX_BATCH_TOKENS is unset
  When embed_max_batch_tokens() is evaluated
  Then it returns 4096
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
HANDLE.RS (lunaris)
  fn parse_batch_tokens(raw: Option<String>, default: u32) -> u32          [new pure fn — testable]
    raw.and_then(|s| s.trim().parse::<u32>().ok()).filter(|&n| n >= 16).unwrap_or(default)
    (empty / non-numeric / <16 -> default; env read stays in the wrappers below so the crate needs
     no env::set_var in tests — edition-2024 unsafe.)

  fn embed_max_batch_tokens() -> u32
    parse_batch_tokens(env("LUNARIS_EMBED_MAX_BATCH_TOKENS"), 4096)
  fn context_embed_max_batch_tokens() -> u32
    parse_batch_tokens(env("LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS"), 1024)

  async fn resolve_embedder(max_batch_tokens: u32) -> Result<Arc<dyn Embedder>, LunarisError>
    [signature change] — the LlamaCppEmbedderOpts sets `max_batch_tokens` from the arg instead of
    `..Default::default()`'s 4096. Remote/Noop branches unchanged (they ignore the budget).
  Lunaris::open (296)      -> resolve_embedder(embed_max_batch_tokens())        // 4096 default
  resolve_default_embedder -> resolve_embedder(context_embed_max_batch_tokens()) // 1024 default

ENV
  LUNARIS_CONTEXT_EMBED_MAX_BATCH_TOKENS  -> contextd embedder budget (default 1024)
  LUNARIS_EMBED_MAX_BATCH_TOKENS          -> general embedder budget (default 4096)

UNCHANGED: n_seq_max (64); reranker budget (8192, never loads via the hook path); the GGUF/remote/
  Noop resolution order; embed output dims; Lunaris::open behavior at its default.
```

Status: FROZEN @ v1 — approved by AI auto-gate (fast-lane freeze; Tin reviews at PR).
Least-sure flag surfaced at freeze: [spec] 1024 tokens may truncate a long (>~4000-char) hook
  capture's tail, slightly weakening that episode's vector (why: budget doubles as the per-input
  token cap via `tokens.truncate(budget)`; cost: bounded — lead tokens carry the signal, raw text
  still stored, env-tunable up). Secondary [contract] resolve_embedder's signature change touches
  both call sites — mechanical, compiler-enforced.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: the pure `parse_batch_tokens` across all 4 scenarios (no env mutation needed).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - parse_batch_tokens_default: parse_batch_tokens(None, 1024) == 1024; (None, 4096) == 4096.
  - parse_batch_tokens_override: parse_batch_tokens(Some("2048"), 1024) == 2048.
  - parse_batch_tokens_bogus_falls_back: (Some(""),1024)==1024; (Some("abc"),1024)==1024;
    (Some("8"),1024)==1024 (below the >=16 floor).
  - context_default_is_1024_general_is_4096: with the env unset in this process, assert
    context_embed_max_batch_tokens()==1024 AND embed_max_batch_tokens()==4096. (Read-only; the CI
    process does not set these vars — no env::set_var.)
</test_plan>

Tests live in: `crates/lunaris/src/handle.rs` (`#[cfg(test)] mod tests`) · MUST run red before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris/src/handle.rs`
Strategy (ordered batches): 1. red: 4 tests over parse_batch_tokens + the two wrappers. 2. add
  `parse_batch_tokens` + `embed_max_batch_tokens` + `context_embed_max_batch_tokens`. 3. change
  `resolve_embedder` to take `max_batch_tokens: u32` (set opts.max_batch_tokens from it). 4. update
  the two call sites (Lunaris::open, resolve_default_embedder). 5. green. 6. live capture_probe.py
  proof (~1.1 GB) + a live recall still returns correct hits.
Safety rule (feature-specific): a bogus/zero env must never yield a degenerate context — the >=16
  floor + the embedder's own `.max(16)` guard both hold. No behavior change for Lunaris::open default.
Code lives in: `crates/lunaris/src/handle.rs`
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

- [x] all tests pass — `lunaris-memory --lib` 84/84 (incl. 4 new parse/budget tests); clippy clean default AND `--no-default-features` (Tier-0 unused-param path guarded)
- [x] coverage did not decrease — added 4 tests, removed none
- [x] no test or contract was altered during build — §3 FROZEN @ v1 as implemented
- [x] the green was EARNED — parse_batch_tokens tests assert real fallbacks (empty/non-numeric/<16) AND the numeric override, not just the happy path; the default test reads the LIVE wrappers (1024/4096). Live footprint proof independently confirms the memory drop.
- [x] concurrency / timing safe — pure functions + one env read at embedder construction; no lock, no `.await` added; the embedder's `budget.max(16)` still guards
- [x] no exposed secrets / injection / unexpected deps — env-var reads only; no new crate
- [x] layering & dependencies follow CONVENTIONS.md — change is confined to `lunaris::handle`; `LlamaCppEmbedderOpts.max_batch_tokens` was already a public field
- [ ] a person reviewed and approved the change — auto-gate (non-security); Tin reviews at PR #55 merge

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
- [x] contextd capture-workload footprint ~1.1 GB (was ~2.5 GB) — LIVE (capture_probe.py, Moon 6381): 1126 MB after 6 scopes × 8 captures, 6×128 MB regions (was 2458 MB / 17 regions). −54%.
- [x] recall still returns correct hits with the 1024-budget embedder — LIVE (posttool_render.py): post_tool recall returns 5 curated hits, 0 raw lines. (prompt-phase 0 hits is the earlier noise fix excluding tool-calls, NOT a regression — confirmed by the post_tool leg working.)
- [x] Lunaris::open unchanged at 4096 — `embed_max_batch_tokens()==4096` test + the general call site passes it; bench/ingest untouched.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `parse_batch_tokens` called by both wrappers; `embed_max_batch_tokens` at Lunaris::open (296); `context_embed_max_batch_tokens` at resolve_default_embedder; `resolve_embedder(max_batch_tokens)` both call sites updated. clippy clean would flag any unused.
- [x] DEAD-CODE (code) — no orphan; all three helpers have callers + tests.
- [x] SEMANTIC — n/a (code task).

### GATE RECORD
Outcome: PASS
Auto-resolved under `autonomy: auto` — Build-expectations confirmed by live evidence (footprint
1126 MB vs 2458; recall 5 hits; general path 4096). Non-security, non-concurrency-residue. Owner
reviews at PR #55 merge.
Reviewed by: AI auto-gate (Tin reviews at PR) · date: 2026-07-14

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>

### Spec delta
Forward changes for the next loop — each re-enters at Specify as the next task. One line
each, tagged `[SPEC · open|seeded|dropped]`, with evidence (e.g. `[SPEC · open] rate-limit
the retry path (evidence: prod herd spikes)`). See the `add` skill's `deltas.md`.
- [SPEC · open] lunaris-mcp (Lunaris::open, 4096) has the same worst-case reservation on heavy
  ingest — 558 MB today but would balloon; consider LUNARIS_EMBED_MAX_BATCH_TOKENS for the mcp
  server too (evidence: same resolve_embedder path).
- [SPEC · open] contextd still ~1.1 GB idle (6×128 MB + model); a further drop needs budget=512
  (809 MB) which risks truncating long captures — revisit if a smaller footprint is required.
- [SPEC · open] the ~453 MB `ps rss` vs 2.5 GB phys_footprint gap means RSS-based memory proofs are
  misleading on macOS — use vmmap phys_footprint (evidence: this investigation's false-flat proof).

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
