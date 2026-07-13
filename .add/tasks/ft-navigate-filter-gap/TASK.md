# TASK: FT.NAVIGATE ignores DSL filters end-to-end (Navigate operator + Moon navigate.rs)

slug: ft-navigate-filter-gap · created: 2026-06-12 · stage: production
phase: done   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: make `.filter(...)`'d Navigate (FT.NAVIGATE) recalls on Moon honor
the filter instead of silently ignoring it.

SPLIT EVIDENCE (recorded verbatim from the moon-hybrid-filter-bypass v1.1
deep-dive, 2026-06-12, verified against moon HEAD 16bc859 + lunaris main):
  - Moon server: src/command/vector_search/navigate.rs has ZERO filter
    handling (grep 'filter|FieldFilter' -> no hits) — FT.NAVIGATE runs KNN
    seeds + graph BFS with no predicate surface.
  - Lunaris port: `StoragePort::vector_navigate` carries no filter param
    (crates/lunaris-storage-moon/src/lib.rs:254-262; raw-RESP path in
    storage-moon/src/navigate.rs).
  - Lunaris operator: operators/navigate.rs `Retriever::retrieve`
    (lines 139-145) never reads ctx.query.filter on the native path;
    the filter only reaches the capability-gated `fallback_vector`
    (lines 109-122) — and THAT filter rendering (`filter_to_moon` TAG) is
    itself dead per the moon-hybrid-filter-bypass §1 evidence.
  -> A `.filter()`'d recall using the Navigate preset returns completely
    unfiltered, graph-expanded results — same silent-wrong-results family as
    the hybrid bypass, on a different retrieval surface.

Framings weighed (decide at contract): interim guard — Navigate with
Some(filter) degrades to fallback_vector + client-side post-filter (small,
Lunaris-only, ships immediately; ranking quality loss when filtered) ·
full fix — FT.NAVIGATE gains the same FILTER clause/HybridFilter allowlist
machinery the hybrid task builds (reuses CHANGE E parser + CHANGE B
allowlist; natural follow-on AFTER moon-hybrid-filter-bypass lands; applies
the allowlist to seeds AND BFS-expanded hits) · document-only (rejected —
silent wrong results, same reason the hybrid task rejected it).
Sequencing note: this task should ride BEHIND moon-hybrid-filter-bypass —
the full fix reuses its Moon-side filter machinery verbatim.

Must:
<must>
  - a `.filter(...)`'d Navigate recall on Moon NEVER returns a hit violating
    the filter (graph-expanded hits included)
  - zero-filter Navigate behavior byte-unchanged
  - regression test: two sources, filtered Navigate recall on live Moon,
    foreign source never surfaces (incl. via BFS expansion from a matching seed)
</must>
Reject:
<reject>
  - silently ignoring the filter (today's behavior) -> the DSL contract is
    the boundary
  - filtering seeds but not BFS-expanded hits -> expansion reintroduces
    foreign sources
</reject>
After:
<after>
  - every Moon retrieval surface Lunaris exposes (vector / keyword / hybrid /
    navigate) enforces DSL filters server-side or via an explicit, documented
    degradation — no silent-ignore path remains
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ BFS-expanded hits can be filtered by the same doc_id allowlist — lowest
    confidence because expanded graph nodes may not all be text-indexed docs
    (key_to_node mapping, atomic.rs:218-221); if a BFS hop lands on a
    non-indexed node the allowlist lookup has no row to check; if wrong: the
    full fix needs per-hit HSET field reads on the expansion path (slower) or
    expansion-time pruning.
    [2026-07-14: MOOT for v1 — the full fix is deferred out of milestone
    scope; the interim guard never filters expanded hits because it never
    expands when filtered.]
  - [x] the Navigate preset is actually reachable with a filter in today's
    production recall presets — confirm at contract; if not reachable, the
    interim guard may be sufficient for v1 and the full fix can wait for
    demand.
    [2026-07-14 RESOLVED: `Navigate::new` has zero call sites outside its own
    module in workspace src/ — reachable only through the public DSL, never a
    built-in preset. Interim guard is sufficient for v1.]
</assumptions>

EVIDENCE REFRESH (2026-07-14, milestone claude-code-flagship adoption, main @ 865b852):
  - moon-hybrid-filter-bypass LANDED since this spec was written: Moon
    `vector_search` now renders DSL filters SERVER-SIDE into the KNN query —
    `({filter_expr})=>[KNN k @vec $query]` via `filter_to_moon`
    (crates/lunaris-storage-moon/src/vector.rs:57-65,126+). The §1 note that
    the fallback's filter rendering is "itself dead" is OBSOLETE — the
    fallback path is now genuinely filter-enforcing on Moon.
    -> the interim guard no longer needs a client-side post-filter.
  - `Navigate::fallback_vector` already passes `ctx.query.filter`
    (operators/navigate.rs:118); the NATIVE path (retrieve(), lines 139-152)
    still ignores it entirely — the hole is confirmed live on today's main.
  - vendor/moon HEAD f9ad681f: src/command/vector_search/navigate.rs still
    has ZERO filter surface — the full fix still requires Moon-side work.
  DECISION for contract: ship the INTERIM GUARD (filter ⇒ route to the
  filter-enforcing vector fallback); record the full FT.NAVIGATE FILTER fix
  as a follow-on outside this milestone (MILESTONE.md Scope Out).

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: filtered Navigate routes to the filter-enforcing vector path
  Given a backend reporting graph_navigate_native = true
  And a query carrying Filter::Eq{ source = "alpha.md" }
  When Navigate::retrieve executes
  Then StoragePort::vector_search is called WITH that exact filter
  And StoragePort::vector_navigate is never called          # reject: silent-ignore

Scenario: zero-filter Navigate byte-unchanged
  Given a backend reporting graph_navigate_native = true
  And a query with no filter
  When Navigate::retrieve executes
  Then vector_navigate serves the hits with hop_depth/vec_score/final_score metadata
  And vector_search is never called                          # must: zero-filter unchanged

Scenario: filtered Navigate on live Moon never leaks a foreign source
  Given one scope holding doc A (source=alpha.md) and doc B (source=beta.md), both embedded near the query
  And a graph edge A -> B written through the PRODUCTION GraphNode/GraphEdge writers
  And an UNFILTERED Navigate recall on this corpus DOES surface B   # discriminator: proves the leak path exists
  When a Navigate recall with Filter::Eq{ source = "alpha.md" } executes against live Moon
  Then every returned hit has source alpha.md
  And B never surfaces — not even via BFS expansion from matching seed A   # reject: seeds-only filtering

Scenario: non-native backend filtered behavior unchanged (regression pin)
  Given a backend reporting graph_navigate_native = false
  And a query carrying a filter
  When Navigate::retrieve executes
  Then vector_search receives the filter (existing capability-gated degradation intact)
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
Navigate::retrieve(ctx) semantics — contract v1 (INTERIM GUARD)

ctx.query.filter == None            (all backends)
  -> byte-unchanged from today:
     native (graph_navigate_native=true): StoragePort::vector_navigate(scope, index, emb, k, spec);
       hits carry hop_depth / vec_score / final_score metadata, score = 1/(1+final_score)
     non-native: fallback_vector (plain vector_search, no filter)

ctx.query.filter == Some(f)         (ALL backends — guard sits ABOVE the capability gate)
  -> route to fallback_vector: vector_search(scope, index, emb, k, Some(f), as_of, false)
     · Moon enforces f SERVER-SIDE: ({filter_to_moon(f)})=>[KNN k @vec $query]
     · hit shape = the existing documented vector-fallback shape (SourceOp::Vector,
       no hop metadata, degraded=false) — same shape non-Moon backends already return
     · emits tracing::debug!(target: "lunaris_retrieve", "navigate: filter present — degraded to filtered vector_search (no graph expansion)")
     · Navigate rustdoc documents the degradation explicitly + names the follow-on full fix

Schema: NO StoragePort change · NO wire change · NO Moon-side change — Lunaris operator only.
Follow-on (out of milestone scope, recorded in MILESTONE.md Out): Moon-side FT.NAVIGATE
FILTER machinery applying the allowlist to seeds AND BFS-expanded hits.

──────────────────────────────────────────────────────────────────────────────
AMENDMENT v1.1 (2026-07-14, change request raised from the RED live test):
the v1 premise "Moon enforces f SERVER-SIDE via ({filter_to_moon(f)})=>[KNN…]"
is FALSE — proven by live probe against vendored Moon (target/moon-flagship,
port 6390):
  · `(@source:{alpha\.md})=>[KNN…]` (what vector.rs sends today): Moon's
    parse_filter_string returns None on the leading '(' → filter SILENTLY
    DROPPED (both docs returned) — vendor/moon ft_search/parse.rs:202-215.
  · bare `@source:{alpha\.md}`: ft_tag_escape's backslashes are compared as
    raw bytes → ZERO hits.
  · bare `@source:{alpha.md}` (no parens, no escaping): WORKS — one hit.
=> EVERY vector_search filter on Moon is silently ignored today; the guard
   would route Navigate into an equally broken path. The fix MUST include the
   Moon KNN filter rendering.

v1.1 adds to the contract (vector.rs, lunaris-storage-moon):
  vector_search(scope, index, emb, k, Some(f), …) on Moon:
    · SERVER-RENDERABLE subset (index=="chunks" only — the sole index with
      TAG/NUMERIC fields): And-composition of
        Eq{field:"source", single-token value without '}'|' '} -> @source:{raw}
        ValidTimeRange -> @valid_time:[lo hi]
      rendered SPACE-JOINED, NO parens, NO escaping ->
      `{expr}=>[KNN {k} @vec $query]` (grammar per Moon parse_filter_string:
      implicit AND, no parens, no OR, no prefix-wildcard).
    · EVERYTHING ELSE (Or, StartsWith, non-chunks index, non-source Eq,
      brace/space values): over-fetch KNN unfiltered (k*4 clamped to [k,1000]),
      client-side post-filter on VectorHit.metadata via filter_matches(),
      truncate to k, tracing::debug the degradation. Unknown (non_exhaustive)
      Filter variants -> StorageError::Backend("filter_unsupported_on_moon…")
      — never silent-drop, never silent-empty.
    · filter_matches(f, meta): Eq = json equality on meta[field] (missing
      field -> false) · StartsWith = str prefix · And/Or recurse ·
      ValidTimeRange over meta["valid_time_ms"].
  The old filter_to_moon/ft_tag_escape unit pins in vector.rs pin the BROKEN
  rendering and are superseded by pins of the v1.1 renderer + evaluator.
  Keyword surface (keyword.rs local filter_to_moon copy, '(f) query' composite
  into ft_text_search): same risk family, SEPARATE surface — recorded as §7
  follow-up, not this task.
──────────────────────────────────────────────────────────────────────────────
```

Least-sure flag surfaced at freeze: [contract] degrade-to-vector over partial graph
filtering — a filtered Navigate loses graph expansion (ranking quality), why: Moon has no
navigate filter surface (vendor/moon f9ad681f navigate.rs, zero filter handling) so any
"filtered expansion" would be client-side theater; cost if wrong: demand for
filtered+expanded recall goes unserved until the Moon-side full fix — mitigated by explicit
rustdoc + tracing + the recorded follow-on.

Status: FROZEN @ v1.1 — v1 + amendment approved by Tin Dang via milestone delegation
2026-07-14 ("act as project owner … you decide implement Lunaris to ship it in limit
timebox now"; precedent: memory-inspector fully-auto delegation). Amendment raised as a
change request from the live RED test — it STRENGTHENS enforcement (fixes the discovered
silent-drop), never weakens a test.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: one test per scenario (4 scenarios -> 4 tests); operator routing logic 100%.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - filtered_navigate_routes_to_filtered_vector_search (RED): mock storage
    (NavRecordingStorage + recorded filter arg), native=true, query.filter=Some(Eq source) /
    retrieve / assert vector_search called once WITH the filter + vector_navigate call count == 0
  - unfiltered_navigate_still_native (pin): native=true, no filter / retrieve / assert
    vector_navigate called + hop metadata intact + vector_search count == 0
  - filtered_navigate_nonnative_keeps_filter (pin): native=false, Some(filter) / retrieve /
    assert vector_search received the filter
  - filtered_navigate_never_leaks_foreign_source_moon (RED, live): MOON_URL-gated skip
    discipline; seed alpha+beta entities + edge alpha->beta via production atomic_write;
    FIRST assert unfiltered Navigate surfaces beta (discriminator — proves
    red-satisfiability); THEN Navigate with Eq{name:"alpha"} (v1.1 post-filter path on the
    entities index): assert beta absent + alpha present
  v1.1 amendment tests:
  - vector.rs unit pins REPLACED: render_knn_filter (source TAG raw, no parens, no escaping;
    And space-join; ValidTimeRange; None for Or/StartsWith/brace-space values) +
    filter_matches evaluator (Eq/StartsWith/And/Or/ValidTimeRange/missing-field)
  - vector_filter_source_tag_moon (RED, live, crates/lunaris-storage-moon/tests/
    vector_filter_moon.rs, moon-it gated): two chunks docs source alpha.md/beta.md;
    vector_search Eq{source:"alpha.md"} returns ONLY the alpha doc (today: parens-drop
    returns both) + Or-filter post-filter path returns both + StartsWith post-filter path
</test_plan>

Tests live in: `crates/lunaris-retrieve/tests/navigate_fallback.rs` (mock scenarios) ·
`crates/lunaris-retrieve/tests/navigate_filter_moon.rs` (live scenario) · MUST run red
(missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): <e.g. debit+credit in one atomic transaction>
Code lives in: `./src/`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — mock suite 7/7 (navigate_fallback) · live navigate 1/1
      (navigate_filter_moon, vendored Moon @6390) · live vector filter 3/3
      (vector_filter_moon: server-side TAG red→green, Or + StartsWith
      post-filter) · storage-moon unit 95/95 · full moon-it live suite green
      (dim_configurable failure = pre-existing shared-server dim stickiness,
      passes on fresh Moon; hybrid_filter-without-MOON_URL failure = rogue
      non-Moon Redis on this box's 6380, IDENTICAL on stashed baseline)
- [x] coverage did not decrease — +4 scenario tests, +10 unit pins replacing
      4 pins of the disproven rendering
- [x] no test or contract was altered during build — the v1.1 amendment was a
      recorded change request (contract phase re-entered) BEFORE build; §4
      tests untouched during build
- [x] concurrency / timing safe — no locks; post-filter HGETs are sequential
      awaits on a cloned typed client; no lock across .await
- [x] no exposed secrets / injection / deps — filter values interpolated into
      the FT query only inside `{…}` with `}`/`{`/space/bool values REJECTED
      to the post-filter path, so a value cannot escape the brace context or
      inject KNN syntax; no new dependencies
- [x] layering — operator guard in lunaris-retrieve; rendering/evaluation in
      lunaris-storage-moon; no cross-layer leaks; keyspace helpers untouched
- [x] reviewed — full-diff self-review under Tin Dang's 2026-07-14 milestone
      delegation (autonomy: auto)

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `render_knn_filter` + `filter_matches` called from
      `vector_search` (vector.rs:84-120); the operator guard is on the
      production `Retriever::retrieve` path (navigate.rs:139-155), reached by
      every DSL Navigate — proven by the live test driving the real
      QueryContext against live Moon through production atomic_write seeding
- [x] DEAD-CODE (code) — deleted `filter_to_moon`/`ft_tag_escape`/
      `json_to_moon`/`json_to_moon_bare` from vector.rs (superseded);
      workspace clippy --all-targets clean confirms no orphans
- [x] SEMANTIC — vendor/moon ft_search/parse.rs::parse_filter_string read in
      full: confirmed leading-'(' abort, raw TAG byte compare, bool/multi-word
      brace semantics, 2-part-numeric/3-part-geo ranges — each encoded as a
      renderer rejection rule + unit pin

### GATE RECORD
Outcome: PASS
Reviewed by: Claude (Fable 5) under Tin Dang's milestone delegation · date: 2026-07-14

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
