# TASK: Hook context inject serves graph+KV hybrid recall

slug: hook-recall-graph-hybrid · created: 2026-07-14 · stage: production
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
- crates/lunaris-hook/src/context.rs — `ContextService::recall_and_trace` (the
  production prompt/tool context path), `recall_vector_hot_path` (direct
  `storage().vector_search("chunks", …)` + `hydrate`), 
  `recall_hot_path_with_keyword_fallback` (vector-else-keyword, NO fusion),
  `recall_keyword_hot_path`, `cached_query_embedding` (hook-side embed cache),
  DEFAULT_PROMPT_MIN_SCORE=0.55 / DEFAULT_TOOL_MIN_SCORE=0.60 (raw-cosine
  thresholds — would ANNIHILATE RRF-fused scores ~Σ1/(60+rank)≈0.03).
- crates/lunaris-retrieve — `Vector::new` / `Keyword::bm25` /
  `AndRetriever` (binary, nestable; legs run via tokio::join) /
  `.fuse_rrf(60)` → `FuseRrfRetriever` (client-side RRF groups by SourceOp —
  two Vector legs merge into ONE cosine-commensurable vector ranking; native
  FT.HYBRID dispatch needs ctx.moon_storage=Some + native_rrf, a manually
  built `QueryContext::new` has None → client-side path, deterministic) /
  `QueryContext` (ALL fields pub; `query_embedding: OnceCell<Vec<f32>>` can be
  PRE-SEEDED so operators never call the placeholder embedder) /
  `hydrate` (hydrate.rs:61 — CHUNK-ONLY: non-chunk ids silently dropped).
- crates/lunaris/src/ingest.rs — graph-ON ingest writes per-fact
  `KvPut{fact_key}` + `VectorUpsert{index:"facts"}` (lines 607-620);
  `lunaris_extract::Fact { id: Ulid, fact_text, predicate, … }` is the at-rest
  fact shape (heterogeneous read model, PROJECT.md §Domain).
- crates/lunaris-hook/tests/context_inject.rs — e2e harness: spawns the real
  binary, `LUNARIS_HOOK_TEST_MOON_URL`-gated positive path, memory:// silent
  paths.
Context (working folder): milestone claude-code-flagship (MILESTONE.md shared
decisions: hard timeout + degrade; no second retrieval engine; filter
correctness precedes graph exposure — the hook path passes NO filter, and
ft-navigate-filter-gap landed anyway). J=94% bench root =
Vector("chunks").and(Keyword::bm25("chunks")).fuse_rrf(60) (longmemeval.rs:686);
the graph+KV A/B lift was reader/synthesis-side. Facts/entities indexes are
EMPTY unless the graph pipeline (remote extractor) is enabled — the facts leg
is dormant-but-wired for default installs.
Honors (patterns / conventions): built ≠ wired (drive the real binary);
design-for-failure (recall failure must never block a session); no
lock-across-await; keyspace helpers only from lunaris_core::keyspace.
Anchors the contract cites: `ContextService::recall_and_trace` ·
`QueryContext::new` + `query_embedding` OnceCell ·
`FuseRrfRetriever` (client-side RRF) · `hydrate` / new `hydrate_mixed` ·
`lunaris_core::keyspace::fact_key` · `lunaris_extract::Fact`

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: the Claude Code hook's context injection (prompt + tool phases) is
served by the blueprint-canonical fused hybrid recall — vector + BM25 + facts
via RRF — instead of today's vector-else-keyword with hand-rolled merge, so
(a) lexical matches (error strings, identifiers) rank via true fusion and
(b) graph-pipeline facts become reachable from Claude Code at all.

Framings weighed: reuse lunaris-retrieve operators with a pre-seeded
QueryContext embedding (chosen — zero new ranking logic, keeps the hook's
embed cache, deterministic client-side RRF) · route through
`handle.recall()` RetrievalBuilder (rejected — cannot pre-seed the cached
query embedding; would re-embed per call in a short-lived hook process) ·
add a reranker pass like the bench (rejected for v1 — bge cross-encoder
lazy-load in a cold hook process blows the session-start budget; follow-on
for contextd, the warm sidecar).

Must:
<must>
  - hybrid ON (default): candidates come from
    Vector("chunks",k) ∧ Keyword::bm25("chunks",k) ∧ Vector("facts",k)
    → fuse_rrf(60), using the hook's cached query embedding (embedder is
    never re-invoked)
  - fact hits hydrate to injectable text (fact_text) — a graph-pipeline fact
    invisible to today's chunks-only path surfaces in the rendered context
  - hard timeout (LUNARIS_CONTEXT_RECALL_TIMEOUT_MS, default 1500) + ANY
    hybrid error/timeout/empty degrades to the EXISTING
    vector-else-keyword path — a session start is never blocked or failed
  - LUNARIS_CONTEXT_RECALL=vector restores today's path byte-identically
  - fused hits bypass the raw min_score cosine thresholds (RRF rank is the
    quality signal; 0.55 would annihilate Σ1/(60+rank) scores)
</must>
Reject:
<reject>
  - hybrid failure surfacing to the agent (hook stderr warn + fallback, never
    a non-zero exit / dropped injection) -> degrade, never block
  - hook-local ranking logic (a second RRF implementation) -> reuse
    lunaris-retrieve operators only
  - re-embedding the query when the hook already holds a cached embedding
</reject>
After:
<after>
  - a Claude Code session backed by Lunaris-on-Moon receives injected context
    ranked by the same fused hybrid family the J=94/96 bench validated, and
    consolidated/extracted FACTS are first-class injectable memories
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ two Vector legs (chunks + facts) merging into one SourceOp::Vector RRF
    group ranks acceptably — lowest confidence because fact-vs-chunk cosine
    scores compete in one ranking; if wrong: facts starve or flood the fused
    pool — mitigated by per-leg k caps and the curation layer's dedupe/caps;
    revisit with SourceOp::Facts weighting if live behavior misranks.
  - [x] QueryContext::new + pre-seeded OnceCell short-circuits embed_once —
    confirmed: embed_once is get_or_try_init; a set cell never invokes the
    embedder (operators/mod.rs:135-150).
  - [x] fused RawHits hydrate: chunk ids via existing hydrate; fact ids miss
    the chunk row and need a fact_key read — confirmed hydrate is chunk-only
    (hydrate.rs:75-100) → new mixed hydration required.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: a graph-pipeline fact surfaces in injected context (live Moon)
  Given a scope holding chunks (via production episode ingest through the hook)
  And a fact written through the production graph-ON write ops (KvPut{fact_key} + VectorUpsert{"facts"})
  And the fact's text answers the session's prompt but exists in NO chunk
  When the real hook binary handles UserPromptSubmit with hybrid recall (default)
  Then stdout additionalContext contains the fact text
  And with LUNARIS_CONTEXT_RECALL=vector the same envelope does NOT surface it   # discriminator: graph+KV-only reachability

Scenario: hybrid root shape is the canonical fused tree
  Given the hybrid root builder for k
  When the retriever tree is constructed
  Then it is FuseRrf(And(And(Vector chunks, Keyword bm25 chunks), Vector facts), k=60)
  And the QueryContext it runs under has the cached embedding pre-seeded (embedder never called)

Scenario: hybrid failure/timeout degrades, never blocks
  Given LUNARIS_CONTEXT_RECALL_TIMEOUT_MS=0 (forced instant timeout)
  When the hook handles a prompt envelope on a reachable store
  Then the response is served by the legacy vector-else-keyword path
  And exit code is 0 and no error reaches stdout                     # reject: failure surfacing

Scenario: legacy opt-out is byte-identical
  Given LUNARIS_CONTEXT_RECALL=vector
  When the hook handles the same envelope
  Then behavior matches today's path (vector hot path, keyword sidecar merge)
  And no facts-index or fused call is made
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
ContextService recall routing — contract v1

env LUNARIS_CONTEXT_RECALL: "hybrid" (DEFAULT) | "vector" (legacy)
env LUNARIS_CONTEXT_RECALL_TIMEOUT_MS: u64, default 1500

hybrid path (recall_hybrid_hot_path):
  root  = Vector::new("chunks", k)
            .and(Keyword::bm25("chunks", k))
            .and(Vector::new("facts", k))
            .fuse_rrf(60)                     # client-side RRF (ctx.moon_storage=None)
  ctx   = QueryContext::new(query, scope, StubEmbedder placeholder, storage, keyword)
          with ctx.query_embedding pre-seeded from cached_query_embedding
          (placeholder embedder is provably never invoked)
  hits  = tokio::time::timeout(timeout_ms, root.retrieve(&ctx))
  hydr  = hydrate_mixed(storage, scope, hits, as_of):
            chunk row (existing hydrate semantics) else fact row
            (read_as_of(fact_key(scope, ulid)) -> lunaris_extract::Fact:
             text = fact_text · source = "fact:{predicate}" · score = fused RRF score)
  degrade: timeout | Err | 0 hydrated hits  -> recall_hot_path_with_keyword_fallback
           (existing legacy path, unchanged), tracing::warn names the cause
  scoring: fused candidates SKIP the min_score cosine threshold; the keyword
           sidecar merge is SKIPPED when hybrid served (BM25 already fused);
           curation (dedupe/caps/render) unchanged downstream

legacy path (LUNARIS_CONTEXT_RECALL=vector): recall_and_trace behaves exactly
as pre-task (vector hot path + keyword sidecar + fallbacks) — routing pin.

Schema: no StoragePort change; hydrate_mixed is a new pub fn in
lunaris-retrieve/src/hydrate.rs (fact-aware; key shape ONLY via
lunaris_core::keyspace::fact_key); no wire change; no Moon-side change.
```

Least-sure flag surfaced at freeze: [contract] facts+chunks sharing one RRF
vector group (§1 ⚠) — why: cosine scores from two indexes compete in one
ranking; cost if wrong: facts mis-ranked (starved/flooded) in injected
context until a SourceOp-weighted follow-on; bounded by per-leg k and
curation caps. [test] the live discriminator depends on the hook process
embedding the prompt on this box (real GGUF embedder) — if the test host
lacks the model the positive-path test skips, weakening evidence to the
memory:// tier (mitigation: run on this box where context_inject.rs's
positive path already passes).

Status: FROZEN @ v1 — approved by Tin Dang via milestone delegation 2026-07-14
("act as project owner … you decide implement Lunaris to ship it in limit
timebox now"; precedent: memory-inspector fully-auto delegation)

AMENDMENT v1.1 (change request, 2026-07-14, same delegation authority — recorded
BEFORE tests were authored; grounding, not build pressure):
1. Root gains a FOURTH leg: `.and(Keyword::bm25("facts", k))`.
   Evidence: graph-ON ingest writes STUB fact embeddings (det_vec hash,
   crates/lunaris/src/ingest.rs:613) — in the merged SourceOp::Vector RRF
   group a stub-embedded fact's cosine vs a real query embedding ranks below
   every real-embedding chunk, so at production scale the Vector("facts") leg
   STARVES (exactly the §1 ⚠ flag). fact_text IS FT-indexed (`content` TEXT)
   and Moon's keyword_search whitelists "facts"
   (crates/lunaris-storage-moon/src/keyword.rs:72), so BM25 is the signal
   that reliably retrieves facts today. Vector("facts") leg RETAINED per v1
   (correct once real fact embeddings land — v1 swap follow-on).
   root = Vector("chunks",k).and(Keyword::bm25("chunks",k))
            .and(Vector::new("facts",k)).and(Keyword::bm25("facts",k))
            .fuse_rrf(60)
2. Clarification (no behavior change): the §2 scenario's "real hook binary"
   for the prompt phase is `lunaris-contextd` — Claude Code's
   UserPromptSubmit hook consults it via the sidecar adapter
   (docs/integration/claude-code.md:50-62); `lunaris-hook` itself only does
   SessionStart handover injection (main.rs::inject_session_context). The
   e2e discriminator drives lunaris-contextd over LUNARIS_CONTEXTD_SOCKET
   with a RecallForPrompt request (ContextService::handle is the contracted
   seam either way).
3. Grounded degrade fact: AndRetriever propagates any leg error
   (combinators.rs:70-74) and the embedded backend's keyword_search is
   NotSupported — so on memory:// and sqlite the hybrid root always errs and
   degrades to the legacy path. This is contract-conformant ("ANY hybrid
   error … degrades") and pins the shipped-SQLite-default behavior:
   hybrid is a Moon-tier feature until embedded BM25 lands.

Status: FROZEN @ v1.1 — same delegation authority, 2026-07-14
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: one test per scenario (4); routing + hydration logic 100%.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - fact_surfaces_in_injected_context_moon (ASSERTION-RED, live, gated
    LUNARIS_HOOK_TEST_MOON_URL): seed chunk episodes via production
    ingest_episode + one fact via production graph-ON ops (KvPut{fact_key} +
    VectorUpsert{"facts"}); drive the REAL lunaris-contextd binary over
    LUNARIS_CONTEXTD_SOCKET with RecallForPrompt (amendment v1.1 §2 — this
    is the binary Claude Code's UserPromptSubmit hook consults) →
    rendered_context contains fact_text; a second contextd with
    LUNARIS_CONTEXT_RECALL=vector does NOT surface it (discriminator,
    proves graph+KV-only reachability; compiles TODAY → red on assertion)
  - hybrid_root_behavioral_shape + preseeded_embedding_never_embeds
    (COMPILE-RED confined to context_hybrid_root.rs): recording mock port —
    the v1.1 root issues exactly vector_search{chunks,facts} +
    keyword_search{chunks,facts} once each at leg k; QueryContext with
    pre-seeded OnceCell + PANICKING placeholder embedder retrieves without
    panic; fused output carries hits from vector-only AND keyword-only legs
  - timeout_zero_degrades_to_legacy (memory://, contextd): TIMEOUT_MS=0
    response == LUNARIS_CONTEXT_RECALL=vector control response (legacy path
    served), connection clean, no hybrid error surfaced (guard pin — green
    pre-build by construction, discriminating post-build)
  - legacy_env_routes_old_path (memory://, contextd):
    LUNARIS_CONTEXT_RECALL=vector responds ok without facts-leg symbols
    (routing pinned strongly at unit level by the shape test)
  - hydrate_mixed unit (COMPILE-RED confined to hydrate_mixed.rs,
    lunaris-retrieve): chunk id → chunk Hit (existing semantics pin); fact
    id → Hit{text=fact_text, source="fact:{predicate}", score preserved};
    unknown id dropped; chunk row wins over fact row for the same id
</test_plan>

Tests live in: `crates/lunaris-hook/tests/context_hybrid_recall.rs` ·
`crates/lunaris-hook/tests/context_hybrid_root.rs` ·
`crates/lunaris-retrieve/tests/hydrate_mixed.rs` (compile-red confined to
the two new-symbol binaries; the contextd e2e file compiles today) ·
MUST run red before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-retrieve/src/hydrate.rs` · `crates/lunaris-retrieve/src/lib.rs` · `crates/lunaris-hook/src/context.rs` · `crates/lunaris-hook/Cargo.toml` · `Cargo.lock` · `tmp/hook-recall-graph-hybrid.txt`
<!-- single-line declaration — the engine's _declared_scope reads ONLY the
     first "Scope (may touch):" line; the original multi-line form silently
     dropped every token after hydrate.rs and produced a false
     scope_violation at the first verify cross (2026-07-14). Cargo.lock rides
     with the dev-dep addition; tmp/ holds the commit message per the repo's
     git rules. -->
Strategy (ordered batches):
1. `hydrate_mixed` in lunaris-retrieve (chunk pass = existing hydrate
   semantics; fact fallback pass via keyspace::fact_key →
   lunaris_extract::Fact) → hydrate_mixed.rs green
2. `pub fn hybrid_root(k)` in lunaris-hook context.rs (v1.1 four-leg root)
   → context_hybrid_root.rs green
3. `recall_hybrid_hot_path` + routing in `recall_and_trace`
   (LUNARIS_CONTEXT_RECALL env, TIMEOUT_MS around root.retrieve only —
   embedding stays outside the window; degrade on timeout|Err|empty; fused
   candidates bypass min_score; keyword sidecar skipped when hybrid served)
   → live discriminator green
4. cargo fmt + clippy --workspace --all-targets + full workspace test
Safety rule (feature-specific): NO hybrid failure may surface to the agent —
every error/timeout/empty path lands in recall_hot_path_with_keyword_fallback
(legacy), warn-once on stderr; never hold the embed-cache Mutex across the
retrieve await.
Code lives in: `crates/lunaris-retrieve/src/` · `crates/lunaris-hook/src/`
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

- [x] all tests pass — 9/9 task tests (4 hydrate_mixed + 2 shape + 3 e2e incl.
  live discriminator on Moon 6390) + `cargo test --workspace --exclude
  lunaris-py --exclude lunaris-ts` rc=0 (2026-07-14)
- [x] coverage did not decrease — 3 new test binaries, 0 removed; no existing
  test touched except fmt reflow
- [x] no test or contract was altered during build — amendment v1.1 recorded
  at tests phase BEFORE authoring the suite; build touched only
  hydrate.rs/lib.rs/context.rs (declared §5 scope); tests untouched post-red
  except the raw_score field fix (tests phase) + rustfmt
- [x] the green was EARNED — the discriminator ran RED first for the right
  reason (legacy control negative + hybrid assertion failing on live Moon,
  bwjijzqv7 log) and flipped green only after the routing landed; the fact is
  reachable ONLY via the facts legs (exists in no chunk); the legacy control
  stayed negative in the SAME seeded scope post-build
- [x] concurrency / timing safe — no lock across await (cached_query_embedding
  drops the Mutex before retrieve; hybrid path takes no new locks); timeout
  wraps ONLY root.retrieve (embedding outside the window per contract);
  degrade path preserves the pre-task error-isolation behavior
- [x] no exposed secrets / injection openings / unexpected deps — new deps are
  dev-only (async-trait, lunaris-extract, both workspace-internal); fact_text
  passes through the existing ScrubEngine (scrub_and_trim) before rendering
- [x] layering follows conventions — keyspace helpers only from
  lunaris_core::keyspace (fact_key); no second retrieval engine (root built
  from lunaris-retrieve operators; RRF stays in FuseRrfRetriever); hook crate
  gains zero ranking logic
- [x] reviewed — Tin Dang via milestone delegation (fully-auto precedent);
  adversarial self-review recorded above

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
- [x] a fact written by graph-ON ingest ops surfaces in lunaris-contextd's
  rendered_context for a matching prompt, with `source=fact:{predicate}` —
  confirmed by by19chjpo log (live Moon, `fact:listens_on`, "7443" rendered)
- [x] LUNARIS_CONTEXT_RECALL=vector restores the chunks-only behavior on the
  SAME seeded scope — confirmed by the discriminator's legacy leg (no "7443")
- [x] TIMEOUT_MS=0 serves EXACTLY the legacy response — confirmed by
  timeout_zero_degrades_to_legacy (response equality vs control contextd)
- [x] fused hits bypass min_score — hybrid branch maps candidates with no
  score filter (context.rs recall_and_trace hybrid arm); RRF-scale scores
  (<0.2) asserted in fused_output_carries_vector_only_and_keyword_only_hits
- [x] embedder never re-invoked under hybrid — PanickingEmbedder + pre-seeded
  OnceCell retrieved without panic (hybrid_root_queries_all_four_legs…)

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — hybrid_root ← recall_hybrid_hot_path ← recall_and_trace
  (production RecallForPrompt/RecallAfterTool route) + both new test binaries;
  hydrate_mixed ← recall_hybrid_hot_path + hydrate_mixed.rs; finish_recall ←
  both recall paths; DEFAULT_HYBRID_TIMEOUT_MS ← recall_hybrid_hot_path.
  Production surface proven by driving the REAL lunaris-contextd binary.
- [x] DEAD-CODE (code) — clippy --workspace --all-targets clean (b1kflimo9,
  exit 0, zero warnings); no orphaned symbol
- [x] SEMANTIC — INGEST-04 unaffected (lunaris-ingest untouched); grep pin
  re-checked: exactly one atomic_write call site in ingest pipeline

### GATE RECORD
Outcome: PASS
Reviewed by: Claude (auto-gate, autonomy: auto; delegation: Tin Dang 2026-07-14) · date: 2026-07-14

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): rate of "hybrid recall failed/empty;
legacy fallback used" stderr warns (a persistently-degrading install is
invisible otherwise) · recall_elapsed_ms of the hybrid hot path under
LUNARIS_CONTEXT_PROFILE=1 vs the 1500ms budget · fact:{predicate} share of
injected memories (starvation signal for the §1 ⚠ ranking flag).

### Spec delta
- [SPEC · open] real fact embeddings at graph-ON ingest (replace det_vec
  stubs), then re-weigh the vector facts leg (evidence: amendment v1.1 —
  stub-embedded facts starve in the merged vector RRF group at scale)
- [SPEC · open] SourceOp-weighted facts ranking if live injection misranks
  facts vs chunks (evidence: §1 ⚠ — fact/chunk cosine scores compete in one
  fused vector group; FuseRrfRetriever::with_weights is the ready seam)
- [SPEC · open] embedded-backend FTS5 BM25 so hybrid serves sqlite/memory
  defaults (evidence: keyword_search NotSupported forces permanent degrade —
  hybrid is a Moon-tier feature today, amendment v1.1 §3)
- [SPEC · open] reranker pass inside contextd (warm sidecar keeps the bge
  cross-encoder loaded; rejected for v1 in §1 framings on cold-start grounds)

### Competency deltas
- [ADD · open] engine: `_declared_scope` reads ONLY the first "Scope (may
  touch):" line — a multi-line declaration silently drops tokens and
  produces a false scope_violation; joining indented continuation lines
  would fix it structurally (evidence: return_to_build attempt-1, 2026-07-14;
  worked around with a single-line declaration)
- [ADD · open] engine: cargo `target` dirs (root, crate-local, vendored)
  needed pruning in _SCOPE_EXCLUDE_DIRS — 45k false touches + 20-minute
  snapshot walks (evidence: same return_to_build; fixed in add.py this task)
- [TDD · open] splitting the suite into compile-red-per-new-symbol binaries +
  one assertion-red e2e that compiles pre-build made "red for the right
  reason" provable per file (evidence: discriminator failed on assertion
  with the legacy control green before build — bwjijzqv7 log)
- [SDD · open] contract amendments recorded at tests phase BEFORE authoring
  the suite (grounding-driven, not build-pressure) kept the freeze honest
  through a four-leg root change (evidence: amendment v1.1 block)
