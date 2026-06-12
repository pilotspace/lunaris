# TASK: Moon hybrid path: pushed-down filter constrains BM25 branch only — dense KNN leaks through RRF

slug: moon-hybrid-filter-bypass · created: 2026-06-12 · stage: production
phase: specify   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: enforce pushed-down `Filter`s on BOTH branches of Moon's native HYBRID
search, so DSL `.filter(...)` users get correct results without per-caller
post-filtering.

SPLIT EVIDENCE (recorded verbatim from scratchpad-handover build, 2026-06-11/12):
  During the handover integration test red run, session B's fresh pad served
  session A's value: `scratchpad_read("plan")` under an empty namespace returned
  `Some("none")` (A's "blocker" value). Root cause traced:
  `crates/lunaris-retrieve/src/fusion.rs:168` —
  `compose_query_with_filter(&ctx.query.text, &ctx.query.filter)` renders the
  filter into the TEXT query passed to `moon hybrid_search` (fusion.rs:260),
  which constrains ONLY the BM25 branch. The dense-KNN branch of Moon's HYBRID
  ignores the text-query predicate, and its foreign-source hits survive RRF
  fusion. Every DSL `.filter()` user on the Moon hybrid path is affected.
  First observed 2026-06-10 ("scratchpad_read-on-Moon" note, recall-optimization
  validation); root-caused and minimally patched 2026-06-12.
  Existing per-caller mitigations (NOT the fix):
    - `lunaris-mcp::tools::recall` post-enforces `source_prefix` after recall.
    - `WorkingMemory::find` post-enforces its source filter
      (`source_filter_matches`, shipped with scratchpad-handover).

DEEPER EVIDENCE (session-context-inject build, 2026-06-12 — live-Moon probe):
  Server-side `source` filtering does not exist on Moon AT ALL, in ANY FT path:
  `FT.SEARCH lunaris_<scope>_chunks_idx "@source:scratchpad*"` returns
  `ERR unknown field 'source'` against moon HEAD (debug build). Moon's FT text
  parser resolves `@field:` ONLY against `text_index.text_fields`
  (vendor/moon/src/server/conn/handler_monoio/ft.rs:238-253); the
  `SchemaField::Tag("source")` that PERF-MOON-01 declares at FT.CREATE
  (crates/lunaris-storage-moon/src/client.rs:376-380) is silently accepted and
  silently unsearchable. The parenthesized composite that keyword.rs/fusion.rs
  build — `"(@source:...) query"` — returns 0 hits without erroring. So:
  - the BM25 branch filter is a NO-OP (not merely "the KNN branch leaks");
  - vector.rs's `filter_to_moon` TAG rendering (`@source:{value}`) is equally
    dead on the vector path — the comment "resolves server-side (PERF-MOON-01)"
    describes an intent Moon never implemented;
  - the real fix is Moon-side: implement TAG-field resolution (or generic
    non-TEXT field predicates) in the FT query parser, then re-validate every
    filter_to_moon rendering against it.
  This RAISES the framing stakes: "lunaris-retrieve-side post-filter" is not a
  partial-quality option but currently the ONLY mechanism that works at all.

Framings weighed (decide at contract): Moon-side fix — HYBRID applies the
filter expression to the KNN candidate set (vendor/moon change; the real fix,
benefits all SDK users) · lunaris-retrieve-side post-filter inside
`fuse_via_moon_native` before RRF normalization (no Moon change; k-starvation
risk: filtered-out hits shrink the fused window) · keep per-caller guards and
document (rejected — silent wrong-results trap for every new DSL user).

Must:
<must>
  - a `.filter(...)`'d hybrid recall on Moon NEVER returns a hit violating the filter
  - fix covers Eq / StartsWith / And / Or / ValidTimeRange on indexed fields
  - k-starvation accounted for: filtering must not silently shrink top-k below
    what a filtered single-branch search would return (fan-out or re-query)
  - regression test on the live-Moon (embedded-moon or conformance) path:
    two sources, filtered hybrid query, foreign source never surfaces
</must>
Reject:
<reject>
  - per-caller post-filtering as the "fix" -> the DSL contract is the boundary
  - dropping the filter push-down entirely -> ranking quality regression
</reject>
After:
<after>
  - `WorkingMemory::find`'s `source_filter_matches` and recall.rs's source_prefix
    post-enforcement become defense-in-depth (retained), not correctness-critical
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Moon's HYBRID command can express a filter on the KNN branch at all — lowest
    confidence because the SDK signature (`hybrid_search(index, q, vec, field,
    sparse, k, weights)`) carries no filter param today; if wrong: the fix needs a
    Moon server + SDK surface change (cross-repo, vendor/moon submodule bump).
  - [ ] the BM25-branch filter rendering (compose_query_with_filter) is itself
    correct for all Filter variants — verify with conformance cases while in there.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

ASSUMPTION SETTLED (2026-06-12):
  The ⚠ assumption — "Moon's HYBRID command can express a filter on the KNN branch
  at all" — is CONFIRMED FALSE. Evidence from vendor/moon source:

  (a) SERVER-SIDE FILTER PATH — DOES NOT EXIST ON THE HYBRID ROUTE:
      vendor/moon/src/command/vector_search/hybrid.rs:52-68 — `HybridQuery`
      struct carries NO filter field. The BM25 branch is called at hybrid.rs:332-340
      as `execute_query_on_index_as_of(text_index, &text_clause, ...)` where
      `text_clause` is produced by `parse_text_query` (hybrid.rs:321-331), NOT
      by `pre_parse_field_filter`. The dense-KNN branch (`run_dense_knn`,
      hybrid.rs:406-500) takes only `(idx, field, blob, k, as_of_lsn, committed)` —
      no filter parameter exists.

      The `FieldFilter::Tag` type DOES exist (ft_text_search.rs:916-923) and
      `TextIndex::search_tag` works correctly (text/store.rs:988-1014), but it is
      only reachable via `pre_parse_field_filter` -> `scatter_text_search_filter`
      (ft.rs:174-208) — a TAG-only FT.SEARCH path that completely bypasses HYBRID.

  (b) WHY `compose_query_with_filter` IS A NO-OP ON BOTH BRANCHES:
      Lunaris builds `"(@source:{val}) text_query"` (fusion.rs:262) and passes it
      as the `text_query` argument to `hybrid_search`. On arrival at the Moon
      server (ft.rs:68-142) the HYBRID path parses the text_query with
      `parse_text_query` (hybrid.rs:321-331). That parser routes `@field:`
      syntax through the TEXT-fields resolver (ft.rs:238-253) which errors with
      `"ERR unknown field 'source'"` because `source` is declared as TAG
      (crates/lunaris-storage-moon/src/client.rs:376-380), not as TEXT.
      Composite queries like `"(@source:{val}) text_query"` start with `(`,
      not `@`, so they do NOT trigger `pre_parse_field_filter` (ft_text_search.rs:
      1105-1106: `if !query.starts_with(b"@") { return Ok(None); }`), and the
      parenthesized prefix is then tokenized as BM25 terms — producing 0 hits
      silently. The KNN branch receives no filter predicate at all.

  (c) SDK SIGNATURE CONFIRMS NO FILTER PARAMETER:
      vendor/moon/sdk/rust/src/text.rs:48-57 — `hybrid_search` signature:
        pub async fn hybrid_search(
            &mut self,
            index: &str,
            text_query: &str,
            query_vec: &[f32],
            vec_field: &str,
            sparse_field: Option<&str>,
            k: usize,
            weights: [f64; 3],
        )
      No filter parameter. The wire format (text.rs:70-98) emits only:
      `FT.SEARCH idx <text_query> HYBRID VECTOR @f $qv FUSION RRF WEIGHTS ...`
      — the `text_query` is the only mechanism through which a filter could
      reach Moon, and it does not work for TAG fields (see (b) above).

  FIX DIRECTION confirmed by evidence:
    The minimal-correct fix adds a `filter: Option<FieldFilter>` to `HybridQuery`
    (Moon) and applies it as a post-filter on both the BM25 candidate set and the
    dense-KNN candidate set inside `execute_hybrid_search_local`, before RRF
    fusion. This is the only approach that (1) covers both branches, (2) requires
    no new protocol tokens for the server-internal path, and (3) ships within the
    existing HybridQuery construction path. The Lunaris side then uses a new Moon
    SDK filter parameter instead of `compose_query_with_filter`.

    Alternative considered — "TAG resolution alone fixes BM25 branch":
    Even if Moon's text parser were taught to route composite queries through
    `pre_parse_field_filter`, that path exits BEFORE BM25 (it short-circuits to
    the TAG-lookup path and returns those docs alone). It cannot feed a filtered
    BM25 stream into HYBRID. The filter must live INSIDE the hybrid execution
    to constrain both streams before fusion.

<scenarios>

```gherkin
# Must 1: a .filter()'d hybrid recall on Moon NEVER returns a hit violating the filter

Scenario: filtered hybrid recall excludes foreign-source documents
  Given a Moon index with chunks from two sources, "scratchpad" and "planning"
  And five chunks from "scratchpad" and five chunks from "planning" are indexed with vectors
  When a hybrid DSL recall is issued with Filter::Eq { field: "source", value: "scratchpad" }
  Then every returned hit has source == "scratchpad"
  And no hit with source == "planning" appears in the result regardless of its RRF rank

# Must 2a: Eq filter (already covered above)
# Must 2b: StartsWith filter

Scenario: StartsWith filter on source prefix constrains both branches of hybrid results
  Given a Moon index with sources "scratchpad:session-1", "scratchpad:session-2", "planning"
  And all three source groups have both text and vector content indexed
  When a hybrid recall is issued with Filter::StartsWith { field: "source", prefix: "scratchpad" }
  Then every returned hit has a source value beginning with "scratchpad"
  And hits with source == "planning" are absent from every position of the result

# Must 2c: And filter

Scenario: And-filter on source and chunk_type constrains hybrid results
  Given a Moon index with chunks of mixed source ("a", "b") and chunk_type ("fact", "note")
  When a hybrid recall is issued with Filter::And([Eq{source,"a"}, Eq{chunk_type,"fact"}])
  Then every returned hit has source == "a" AND chunk_type == "fact"
  And hits matching only one condition are absent from the result

# Must 2d: Or filter

Scenario: Or-filter accepts hits from either branch source and rejects the third
  Given a Moon index with chunks from sources "alpha", "beta", "gamma"
  When a hybrid recall is issued with Filter::Or([Eq{source,"alpha"}, Eq{source,"gamma"}])
  Then every returned hit has source == "alpha" OR source == "gamma"
  And hits with source == "beta" are absent

# Must 2e: ValidTimeRange filter

Scenario: ValidTimeRange filter on valid_time bounds both hybrid branches
  Given a Moon index with chunks having valid_time ranges spanning 2025 and 2026
  When a hybrid recall is issued with a ValidTimeRange filter covering only 2025
  Then every returned hit has a valid_time interval intersecting 2025
  And chunks whose valid_time is entirely within 2026 are absent

# Must 3: k-starvation — filtered hybrid top-k must not be silently starved

Scenario: filtered hybrid top-k is not silently starved below filtered single-branch count
  Given a Moon index with 20 chunks all from "scratchpad" with both text and vector content
  And k = 10 is requested
  When a hybrid recall is issued with Filter::Eq { field: "source", value: "scratchpad" }
  Then the result contains at least as many hits as a filtered BM25-only FT.SEARCH
    for the same filter and same k would return
  And the result count equals min(10, total_matching_chunks)

# Must 4: live-Moon two-source regression

Scenario: live-Moon two-source conformance — foreign source never surfaces
  Given an embedded-Moon or conformance Moon instance (via lunaris-mcp embedded-moon feature)
  And two document sets are ingested: source "A" with 10 chunks, source "B" with 10 chunks
  And both sets have embeddings indexed on the same vector field
  When a hybrid_search is issued with a filter scoped to source "A" only
  Then no chunk with source "B" appears in any position of the result
  And the result contains only source-"A" chunks, up to k

# Reject 1: per-caller post-filtering is NOT the fix — DSL contract is the boundary

Scenario: filter correctness does not depend on per-caller post-enforcement
  Given the WorkingMemory::find source_filter_matches post-filter is bypassed (test harness flag)
  And lunaris-mcp recall source_prefix post-enforcement is bypassed (test harness flag)
  When a hybrid DSL recall is issued with Filter::Eq { field: "source", value: "scratchpad" }
  Then the result still contains only scratchpad chunks
  And the per-caller guards are not the mechanism achieving correctness

# Reject 2: dropping filter push-down entirely causes ranking quality regression

Scenario: no-filter baseline is unaffected — RRF ranking quality is preserved
  Given a Moon index with 20 mixed-source chunks
  When a hybrid DSL recall is issued with NO filter
  Then results are ranked by RRF fused score across BM25 and dense-KNN branches
  And the result is equivalent in score ordering to the pre-fix unfiltered hybrid result
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
INTERNAL CONTRACT — no HTTP surface change; this is a cross-repo internal fix.
Scope: Moon server + Moon Rust SDK (vendor/moon submodule) + lunaris-retrieve.

────────────────────────────────────────────────────────────────────────────────
Moon-side changes (vendor/moon — land in pilotspace/moon first, then bump SHA):
────────────────────────────────────────────────────────────────────────────────

FILE: vendor/moon/src/command/vector_search/hybrid.rs

  CHANGE A — HybridQuery gains an optional filter field:
    pub filter: Option<crate::command::vector_search::ft_text_search::FieldFilter>
    (FieldFilter already exists at ft_text_search.rs:916-923; no new type needed)

  CHANGE B — execute_hybrid_search_local (hybrid.rs:289) applies the filter
    as a pre-RRF post-filter on BOTH streams AFTER collection and BEFORE
    rrf_fuse_three:
      - BM25 branch: after text_results: Vec<TextSearchResult> is collected
        (hybrid.rs:332-341), apply filter by building a doc_id allowlist via
        TextIndex::search_tag (or search_numeric_range) and retaining only
        matching TextSearchResult entries.
      - Dense-KNN branch: after dense_results: Vec<SearchResult> is collected
        (hybrid.rs:344-358), resolve each result's key via key_hash_to_key,
        then look up the doc_id via text_index.key_hash_to_doc_id (or equivalent
        reverse map) and intersect against the same allowlist. Hits whose key is
        not found in the text index (pure-vector-only docs) are retained only if
        they pass the filter by direct field lookup from the raw HSET data (see
        NOTE below on field storage scope).
      - Sparse branch (when present): same doc_id intersection against allowlist.
    NOTE: the filter allowlist is computed ONCE from the text index before either
    stream loop; it is a roaring::RoaringBitmap or Vec<u32> of matching doc_ids.

  CHANGE C — k_per_stream fan-out when filter is present:
    In effective_k_per_stream() (hybrid.rs:78), when query.filter.is_some(),
    return max(k_per_stream_default, 5 * top_k, 60) instead of 3x, to compensate
    for expected filter attrition on the raw candidate window (k-starvation Must).
    The 5x multiplier is the default; callers may override via explicit K_PER_STREAM.

  CHANGE D — HybridQuery construction in ft.rs:100-111 plumbs the filter field
    (initially None until wire protocol is extended — CHANGE E below):
    hq = HybridQuery { ..., filter: None };

FILE: vendor/moon/src/command/vector_search/ft_search/ (parse.rs or mod.rs)

  CHANGE E — New FT.SEARCH wire modifier FILTER parsed during HYBRID dispatch
    (ft.rs:68-142). Syntax appended after FUSION RRF [...]:
      FILTER TAG @<field> <value>        (maps to FieldFilter::Tag)
      FILTER NUMERIC @<field> [min max]  (maps to FieldFilter::NumericRange)
    The parser scans for the FILTER keyword after the HYBRID block tokens;
    if found, constructs a FieldFilter and sets HybridQuery.filter.
    Unknown FILTER subtypes return Frame::Error("ERR unsupported FILTER type").
    When FILTER is absent, HybridQuery.filter = None (backward compatible).

FILE: vendor/moon/src/shard/scatter_hybrid.rs

  CHANGE F — scatter_hybrid_search threads HybridQuery.filter to each per-shard
    execute_hybrid_search_local call (already carried in HybridQuery; no signature
    change needed beyond CHANGE A propagation).

FILE: vendor/moon/sdk/rust/src/text.rs

  CHANGE G — hybrid_search gains an optional filter parameter:
    pub async fn hybrid_search(
        &mut self,
        index: &str,
        text_query: &str,      // pure BM25 query text — no longer embeds filter
        query_vec: &[f32],
        vec_field: &str,
        sparse_field: Option<&str>,
        k: usize,
        weights: [f64; 3],
        filter: Option<HybridTagFilter>,   // new — None is fully backward compat
    )
    where HybridTagFilter is a new minimal SDK enum:
      pub enum HybridTagFilter {
          Tag { field: String, value: String },
          Numeric { field: String, min: f64, max: f64 },
      }
    Wire encoding: when filter is Some(HybridTagFilter::Tag { field, value }),
    appends `FILTER TAG @{field} {value}` to the FT.SEARCH command after
    the FUSION RRF clause. When None, wire is unchanged (backward compat).

────────────────────────────────────────────────────────────────────────────────
Lunaris-side changes (after Moon submodule bump + moondb crate version update):
────────────────────────────────────────────────────────────────────────────────

FILE: crates/lunaris-retrieve/src/fusion.rs

  CHANGE H — fuse_via_moon_native removes compose_query_with_filter:
    The call at fusion.rs:168 is DELETED. text_query is passed as-is
    (ctx.query.text unchanged). The filter is passed via the new SDK parameter:
      text.hybrid_search(
          &per_scope_index,
          &ctx.query.text,      // no longer prefixed with filter expression
          &q_emb,
          "vec",
          None,
          k,
          weights,
          filter_to_moon_hybrid_filter(ctx.query.filter.as_ref()),  // new
      )

  CHANGE I — new private function filter_to_moon_hybrid_filter:
    fn filter_to_moon_hybrid_filter(f: Option<&Filter>) -> Option<moondb::HybridTagFilter>
    Translates Lunaris Filter variants to moondb::HybridTagFilter:
      Filter::Eq { field, value }          -> HybridTagFilter::Tag { field, value }
      Filter::StartsWith { field, prefix } -> HybridTagFilter::Tag { field,
                                               value: prefix (Moon TAG prefix match
                                               via {prefix*} wire encoding) }
      Filter::And / Or                     -> the first Tag-typed leaf is used (v1);
                                             compound filter support deferred to v2
                                             (see "Least-sure flag" below)
      Filter::ValidTimeRange               -> None for now (text_index numeric field
                                             for valid_from/valid_to is not yet declared
                                             at FT.CREATE; add as a follow-on task)
    Returns None when the filter variant has no Moon HybridTagFilter equivalent;
    the Lunaris caller-side post-filter remains active as defense-in-depth for
    those cases (per §1 After).

  CHANGE J — compose_query_with_filter is no longer called from fuse_via_moon_native.
    Verify no other callers in the crate; if no callers remain, mark deprecated
    with a doc comment pointing to filter_to_moon_hybrid_filter as the replacement.

FILE: crates/lunaris-storage-moon/src/vector.rs

  CHANGE K — re-validate filter_to_moon TAG rendering (@source:{value}):
    Now that TAG filtering is real server-side (via FT.SEARCH TAG-only path),
    verify the ft_tag_escape formatting used by filter_to_moon matches what Moon's
    TAG parser expects. The HYBRID path uses HybridTagFilter directly (not the
    text-query encoding), so this is a consistency check, not a logic change.

────────────────────────────────────────────────────────────────────────────────
Invariants:
────────────────────────────────────────────────────────────────────────────────
  - compose_query_with_filter MUST NOT be called from fuse_via_moon_native after
    CHANGE H. Verified by: grep -c 'compose_query_with_filter' \
    crates/lunaris-retrieve/src/fusion.rs (must return 0 for production call sites;
    definition line + test lines are acceptable).
  - The Moon-side CHANGE B filter application occurs AFTER stream collection and
    BEFORE rrf_fuse_three — never inside HNSW graph traversal or BM25 scoring.
  - Per-caller guards (WorkingMemory::find source_filter_matches, recall.rs
    source_prefix) are RETAINED as defense-in-depth — NOT removed.
  - Moon submodule is bumped ONLY to a SHA that has been pushed to pilotspace/moon
    (prior CI breakage: "not our ref" from unpushed SHA).
  - CHANGE A backward compat: callers passing filter: None observe identical
    behavior to the pre-fix hybrid_search call.

────────────────────────────────────────────────────────────────────────────────
Evidence protocol (red today / green after):
────────────────────────────────────────────────────────────────────────────────
  RED — against Moon HEAD (debug build, port 6380):
    redis-cli -p 6380 FT.SEARCH lunaris_dev_chunks_idx \
      "@source:{scratchpad}" LIMIT 0 1
    -> ERR unknown field 'source'

    redis-cli -p 6380 FT.SEARCH lunaris_dev_chunks_idx \
      "(@source:{scratchpad}) hello" LIMIT 0 1
    -> 0 hits (silent NO-OP — query tokenized as BM25 terms, no error)

    cargo test -p lunaris-retrieve \
      -- test_filtered_hybrid_excludes_foreign_source
    -> FAIL (foreign source hits appear in result)

  GREEN — after Moon changes + submodule bump + Lunaris CHANGE H:
    redis-cli -p 6380 FT.SEARCH lunaris_dev_chunks_idx \
      "hello" HYBRID VECTOR @vec $qv FUSION RRF \
      FILTER TAG @source scratchpad PARAMS 2 qv <blob> LIMIT 0 10
    -> returns only scratchpad-sourced hits

    cargo test -p lunaris-retrieve \
      -- test_filtered_hybrid_excludes_foreign_source
    -> PASS

────────────────────────────────────────────────────────────────────────────────
Conformance / regression tests:
────────────────────────────────────────────────────────────────────────────────
  - /crates/lunaris-retrieve/tests/hybrid_filter.rs (new) — integration test
    using embedded-moon; two sources; asserts no foreign-source hit survives
    across all Filter variants handled in CHANGE I (Eq, StartsWith).
  - /crates/lunaris-retrieve/tests/hybrid_filter_k_starvation.rs (new) —
    all-matching-source corpus of 20 chunks, k=10; result count == 10.
  - /crates/lunaris-retrieve/tests/hybrid_filter_and_or.rs (new) — And / Or
    compound filter scenarios (defense-in-depth via post-filter; confirms no
    regression in the And/Or cases where v1 translates only the first leaf).
  - vendor/moon/tests/hybrid_filter_tag.rs (new, Moon-side) — unit test of
    execute_hybrid_search_local with filter=Some(FieldFilter::Tag{...}); asserts
    foreign-source vectors are absent from fused output from BOTH BM25 and KNN.
  - vendor/moon/tests/hybrid_filter_backward_compat.rs (new, Moon-side) —
    filter=None produces identical output to pre-CHANGE-A hybrid_search call.
  - /crates/lunaris-mcp/tests/server_boot.rs — existing server-boot roster test
    remains green (no new MCP tools in this task).
  - Existing per-caller guard tests (WorkingMemory::find, recall.rs) remain
    green and are NOT removed.

────────────────────────────────────────────────────────────────────────────────
Cross-repo sequencing:
────────────────────────────────────────────────────────────────────────────────
  1. Moon PR to pilotspace/moon: CHANGE A + B + C + D + E + F + G + Moon tests.
     Spike CHANGE E (FILTER wire modifier parser) as a standalone commit first to
     validate the protocol extension before threading HybridQuery.filter end-to-end.
  2. Moon PR merged -> git submodule update --remote vendor/moon in lunaris repo.
  3. Lunaris PR: CHANGE H + I + J + K + lunaris-retrieve tests.
  4. Never pin a vendor/moon SHA not yet pushed to pilotspace/moon.
```

Status: DRAFT

Least-sure flag for freeze:
  [contract] CHANGE I — compound filter (And / Or) v1 degrades to first-leaf-only
    for the Moon HybridTagFilter translation: the risk is that a Filter::And(
    [Eq{source,"A"}, Eq{chunk_type,"fact"}]) only enforces the source predicate
    via the server-side FILTER TAG, and the chunk_type predicate is handled by the
    remaining Lunaris post-filter (defense-in-depth). This is a correctness gap:
    chunks matching source "A" but NOT chunk_type "fact" can survive from the
    dense-KNN branch if the post-filter is ever disabled.
    Cost if not addressed before freeze: the And/Or scenarios (Must 2c/2d) require
    the post-filter to remain correctness-critical for compound filters, which
    contradicts the §1 Reject ("per-caller post-filtering is NOT the fix").
    Mitigation path: extend HybridTagFilter to an enum tree (Tag / Numeric / And /
    Or) matching the Lunaris Filter tree and extend the Moon FILTER wire syntax to
    support compound predicates — this is v2 scope; v1 ships the Eq + StartsWith
    coverage, which resolves the production-observed scratchpad-isolation bug.
    The freeze must acknowledge this scope boundary explicitly.

<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1-2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: <e.g. 90%>
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_<scenario>: arrange <Given> / act <When> / assert <Then> + assert <unchanged>
</test_plan>

Tests live in: `./tests/` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./...` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with + · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): <e.g. debit+credit in one atomic transaction>
Code lives in: `./src/`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build
- [ ] concurrency / timing of the risky operation is safe
- [ ] no exposed secrets, injection openings, or unexpected dependencies
- [ ] layering & dependencies follow CONVENTIONS.md
- [ ] a person reviewed and approved the change

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [ ] WIRING (code) — every new symbol is referenced; record where / how confirmed
- [ ] DEAD-CODE (code) — no new unused or orphaned symbol introduced
- [ ] SEMANTIC (prose / non-code) — read in full, not skimmed: <what read · what confirmed>

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
If RISK-ACCEPTED -> owner: <name> · ticket: <link> · expires: <date>   (never for a security gap)
Reviewed by: <name> · date: <date>

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
