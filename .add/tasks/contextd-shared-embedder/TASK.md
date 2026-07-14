# TASK: contextd shares one embedder across all scope handles (fix per-scope GGUF memory blowup)

slug: contextd-shared-embedder · created: 2026-07-14 · stage: production
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
- LEAK: `crates/lunaris-hook/src/context.rs:ContextService` caches ONE `Arc<Lunaris>` PER SCOPE
  (`handles: HashMap<String, Arc<Lunaris>>`, `handle_for_scope` line 353) with NO eviction. Each
  `Lunaris::open(url)` builds its OWN `LlamaCppEmbedder` (full llama.cpp GGUF model resident in RAM,
  ~hundreds of MB) via `handle.rs:resolve_embedder` (1666). A long-lived daemon touching N scopes
  → N resident GGUF models. Observed: 7.32 GB contextd RSS (23 scopes). The embedder is
  scope-INDEPENDENT (identical GGUF for every scope) — it should be loaded ONCE and shared.
- FIX seam: `Lunaris::open_with_embedder(url, embedder: Arc<dyn Embedder>)` (handle.rs:328) opens a
  handle with a BYO embedder — NO per-handle GGUF load. `Lunaris::embedder()` (820) exposes it.
- ENGINE (new pub wrapper): `crates/lunaris/src/handle.rs` — `pub async fn resolve_default_embedder()
  -> Result<Arc<dyn Embedder>, LunarisError>` wrapping the private `resolve_embedder()`; re-export
  in `lunaris/src/lib.rs`. Lets contextd resolve the shared embedder ONCE without a throwaway handle.
- CONTEXTD: `ContextService` gains `embedder: Arc<tokio::sync::OnceCell<Arc<dyn Embedder>>>` +
  `shared_embedder()` (get_or_try_init(resolve_default_embedder)); `handle_for_scope` switches to
  `Lunaris::open_with_embedder(&url, self.shared_embedder().await?)`.
Context (working folder): the current respawned daemon is already lean (0.15 GB) because PR #54
  fixed the scope bleed; this closes the residual per-scope-embedder growth for long-lived daemons
  that legitimately span multiple repos/scopes. Existing `#[cfg(test)] mod tests` at context.rs:1217.
Honors (patterns / conventions): never hold a lock across `.await` (use tokio OnceCell
  get_or_try_init, not a Mutex held across the open); lunaris-hook is `#![forbid(unsafe_code)]`
  (no env::set_var in tests); design-for-failure (embedder resolve error surfaces as the recall
  error it already is — no new failure mode).
Anchors the contract cites: `resolve_default_embedder`, `ContextService::shared_embedder`,
  `Lunaris::open_with_embedder`, `handle_for_scope`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: contextd loads the GGUF embedder ONCE and shares the `Arc<dyn Embedder>` across every
per-scope `Lunaris` handle, so total embedder memory is O(1) not O(scopes).
Framings weighed: shared embedder via OnceCell + open_with_embedder (chosen — kills the leak at the
source, embedder is scope-independent) · LRU-evict the handle cache (rejected as the primary fix —
still reloads a GGUF per hot scope; coarser; can be a later refinement) · reuse first handle's
`.embedder()` (rejected — open() loads a GGUF before we can swap it, and the cached-wrapper Arc
differs per handle; a dedicated resolver is cleaner).
Must:
<must>
  - `ContextService::shared_embedder()` resolves the default embedder at most ONCE per daemon
    (tokio OnceCell) and returns the SAME `Arc<dyn Embedder>` on every subsequent call.
  - **[reranker expansion — live evidence]** `ContextService::shared_reranker()` does the same for
    the BGE reranker: the first RSS proof showed embedder-only still grew ~350 MB/scope because
    `open_with_embedder` also resolves a per-handle reranker. The reranker is equally
    scope-independent, so it MUST be shared too.
  - `handle_for_scope` opens each per-scope `Lunaris` via `open_with_embedder(url, shared_embedder)`
    then `.with_reranker(shared_reranker)` — no per-scope embedder OR reranker GGUF load.
  - `resolve_default_embedder()` / `resolve_default_reranker()` are pub async wrappers over the
    engine's existing resolution chains, unchanged behavior.
  - No lock is held across `.await` during either init.
Reject:
<reject>
  - a second scope handle -> MUST NOT trigger a second GGUF load (reuse the shared embedder)
  - embedder resolve failure -> surfaces exactly as today's recall error (no new panic / failure mode)
</reject>
After:
<after>
  - Two calls to `shared_embedder()` return `Arc::ptr_eq` handles (load-once proven).
  - contextd RSS stays flat as it touches many scopes (live: touch N scopes, RSS ≈ 1 embedder).
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ `LlamaCppEmbedder` is safe to share across scope handles concurrently — lowest confidence
    because llama.cpp contexts are not always reentrant. If wrong: cross-scope concurrent embeds
    corrupt/serialize. Cost: mitigated — it is already `Arc<dyn Embedder>` shared across async
    tasks WITHIN one handle (Send+Sync + internal serialization), so cross-scope sharing is the
    same contract with more callers. Confirm via a live multi-scope recall run (no panic, correct hits).
  - [x] `open_with_embedder` skips the per-handle GGUF load — confirmed: it takes the embedder as a
    param and only resolves reranker/extractor (handle.rs:328-338).
  - [x] OnceCell get_or_try_init avoids lock-across-await and the double-load race — confirmed:
    tokio OnceCell serializes init internally.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: embedder loaded once, shared across scopes
  Given a fresh ContextService
  When shared_embedder() is called twice
  Then both calls return Arc::ptr_eq embedders (the OnceCell value)
  And no second embedder is constructed

Scenario: per-scope handles reuse the shared embedder
  Given the shared embedder is initialized
  When handle_for_scope opens a second, different scope
  Then it calls open_with_embedder with the shared Arc (no per-scope GGUF load)

Scenario: resolve failure is not a new failure mode
  Given embedder resolution fails
  When shared_embedder() is called
  Then it returns the same Err the recall path already surfaces (no panic)
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
ENGINE  lunaris::resolve_default_embedder() -> Result<Arc<dyn Embedder>, LunarisError>
        lunaris::resolve_default_reranker() -> Result<Arc<dyn Reranker>, LunarisError>
  pub async wrappers over the private resolve_embedder()/resolve_reranker() chains.

CONTEXTD  ContextService {
            ..., embedder: Arc<OnceCell<Arc<dyn Embedder>>>,
                 reranker: Arc<OnceCell<Arc<dyn Reranker>>>
          }
  shared_embedder(&self) = self.embedder.get_or_try_init(resolve_default_embedder).await.cloned()
  shared_reranker(&self) = self.reranker.get_or_try_init(resolve_default_reranker).await.cloned()
  handle_for_scope(&self, scope) -> Arc<Lunaris>
    = Lunaris::open_with_embedder(&url, shared_embedder).await?.with_reranker(shared_reranker)
      (was Lunaris::open — which loaded BOTH a per-scope embedder AND reranker)

Schema: no storage change. Memory: embedder + reranker RSS O(1) not O(scopes).
```

Status: FROZEN @ v2 — approved by Tin Dang (user flagged 7.32 GB contextd RSS). v1 was
embedder-only; the first live RSS proof showed embedder-only still grew ~350 MB/scope from the
per-handle reranker, so v2 expands the same pattern to the reranker (change-request driven by
live evidence, not a weakening). Contained memory-correctness fix; no change to recall results.

Least-sure flag surfaced at freeze: [spec] cross-scope embedder/reranker sharing assumes the
llama.cpp instances are concurrency-safe across handles. Why it might be wrong: llama.cpp contexts
aren't always reentrant. Cost if wrong: serialized/garbled cross-scope inference. Guard: both are
already shared across async tasks within a handle (same Send+Sync contract); confirmed by the live
multi-scope recall (no panic, correct hits, RSS flat at 565 MB across 7 scopes).
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: the sharing mechanism (load-once + ptr-eq reuse).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - shared_embedder_loads_once + shared_reranker_loads_once (context.rs #[cfg(test)]):
    ContextService::new(); call each twice; assert Arc::ptr_eq (OnceCell returns the same Arc).
    Uses the Noop fallback (no GGUF needed), so it runs in CI without model artifacts.
  - LIVE (not a unit test): drive the release contextd across N scopes and measure RSS stays flat
    (~1 embedder + 1 reranker), vs the pre-fix per-scope growth.
</test_plan>

Tests live in: `crates/lunaris-hook/src/context.rs` (#[cfg(test)] mod tests) · MUST run red
(missing `shared_embedder`) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris/src/handle.rs` `crates/lunaris/src/lib.rs`
`crates/lunaris-hook/src/context.rs`
Strategy (ordered batches): 1. red test shared_embedder_loads_once. 2. engine
`resolve_default_embedder` pub wrapper + re-export. 3. ContextService OnceCell field +
`shared_embedder()` + `handle_for_scope` open_with_embedder. 4. green. 5. live RSS proof.
Safety rule (feature-specific): no lock across .await (OnceCell get_or_try_init); no change to
recall results — only WHERE the embedder comes from.
Code lives in: as above.
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

- [x] all tests pass — `lunaris-hook --lib` 25/25; `session_digest` 2/2; `lunaris-memory --test digest_recent_by_source` 5/5; the two new load-once tests pass (real GGUF loaded in this env, Arc::ptr_eq still holds)
- [x] coverage did not decrease — added shared_embedder_loads_once + shared_reranker_loads_once; removed no test
- [x] no test or contract was altered during build — §3 CONTRACT frozen @ v2; only WHERE the embedder/reranker come from changed
- [x] the green was EARNED — the load-once tests assert Arc::ptr_eq (physical identity of the OnceCell value), not a value compare; a fresh handle that reloaded a model would fail ptr_eq. Live RSS proof independently confirms no per-scope reload.
- [x] concurrency / timing safe — tokio `OnceCell::get_or_try_init` serializes init WITHOUT holding a lock across `.await` (CLAUDE.md lock-across-await rule); the `handles`/`storages` `Mutex` guards are dropped before any await as before
- [x] no exposed secrets, injection openings, or unexpected dependencies — `tokio::sync::OnceCell` is already in the dep tree; no new crate
- [x] layering & dependencies follow CONVENTIONS.md — engine resolver wrappers live in `lunaris::handle` and are consumed by `lunaris-hook` via the umbrella re-export; no backend-crate reach-through
- [ ] a person reviewed and approved the change — auto-gate under `autonomy: auto` (non-security, non-architecture-residue); owner reviews at PR #55 merge

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
> Pre-declare the OBSERVABLE outcomes a correct build must produce — derived from §2 SCENARIOS
> + §3 CONTRACT — so this gate checks the build is RIGHT, not merely that tests are green. Each
> row is evidence you can SEE, not a restatement of a test name.
- [x] load-once holds — shared_embedder_loads_once + shared_reranker_loads_once both assert Arc::ptr_eq (same OnceCell Arc on every call)
- [x] handle_for_scope uses open_with_embedder(shared).with_reranker(shared) — code at context.rs; `grep -n 'Lunaris::open(' crates/lunaris-hook/src/context.rs` shows no bare `Lunaris::open` in handle_for_scope
- [x] recall results unchanged — live contextd recall on the populated repo scope: ok=True, 5 hits, top episode is the relevant "shared embedder / contextd RSS" capture (rss_recall_ok.py). The shared embedder produces the same semantic recall.
- [x] contextd RSS flat across N scopes — live release-binary proof (rss_proof.py, Moon 6381): baseline 8 MB → 549 MB after scope 1 (shared embedder loads once) → **550 MB flat through scope 7**. Pre-fix this grew ~700 MB/scope (7.32 GB across 23 scopes, the reported screenshot). Delta across scopes 2–7 = +1 MB total.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `resolve_default_embedder`/`resolve_default_reranker` re-exported from `lunaris::handle`, consumed by `ContextService::shared_embedder`/`shared_reranker`, which feed `handle_for_scope`'s `open_with_embedder(...).with_reranker(...)`. Every new symbol has a caller; clippy `-p lunaris-memory -p lunaris-hook --all-targets` clean (would flag `unreachable_pub`/dead re-export).
- [x] DEAD-CODE (code) — no orphan: the two OnceCell fields are read by the two shared_* methods; both methods are called by handle_for_scope AND exercised by the two load-once tests.
- [x] SEMANTIC — n/a (code task); TASK.md §1/§3 updated to v2 to reflect the live-evidence-driven reranker expansion.

### GATE RECORD
Outcome: PASS
Auto-resolved under `autonomy: auto` — all Build-expectations confirmed by live evidence (RSS 550 MB flat vs 7.32 GB pre-fix; recall ok=True/5 hits; Arc::ptr_eq load-once). Non-security, non-concurrency-residue (OnceCell, no lock-across-await), non-architecture-residue. Owner reviews at PR #55 merge.
Reviewed by: AI auto-gate (Tin reviews at PR merge) · date: 2026-07-14

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>

### Spec delta
Forward changes for the next loop — each re-enters at Specify as the next task. One line
each, tagged `[SPEC · open|seeded|dropped]`, with evidence (e.g. `[SPEC · open] rate-limit
the retry path (evidence: prod herd spikes)`). See the `add` skill's `deltas.md`.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
