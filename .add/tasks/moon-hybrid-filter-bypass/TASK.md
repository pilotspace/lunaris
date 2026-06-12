# TASK: Moon hybrid path: pushed-down filter constrains BM25 branch only — dense KNN leaks through RRF

slug: moon-hybrid-filter-bypass · created: 2026-06-12 · stage: production
phase: build   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
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
  at all" — is CONFIRMED FALSE. Evidence below RE-VERIFIED 2026-06-12 (second
  pass) against ../moon working HEAD (16bc859, feat/shardslice-migration):
  `git diff c72c5861..HEAD` over the hybrid-path files is EMPTY — vendor/moon
  v0.3.0 and moon HEAD are byte-identical here (the shardslice migration
  touched shard routing elsewhere). Line numbers below are the verified
  moon-HEAD values (the first draft's citations drifted; corrected).

  (a) SERVER-SIDE FILTER PATH — DOES NOT EXIST ON THE HYBRID ROUTE:
      moon src/command/vector_search/hybrid.rs:52-69 — `HybridQuery` struct
      carries NO filter field (fields: index_name, text_query, dense_field,
      dense_blob, sparse, weights, k_per_stream, top_k, offset, count).
      `execute_hybrid_search_local` (hybrid.rs:300) calls the BM25 branch at
      hybrid.rs:343-351 as `execute_query_on_index_as_of(text_index,
      &text_clause, None, None, k, as_of_lsn)` — the two `None`s are
      global_df/global_n (distributed BM25 stats, ft_text_search.rs:1617-1624),
      NOT a filter slot. The dense-KNN branch (`run_dense_knn`, hybrid.rs:417)
      takes only `(idx, field, blob, k, as_of_lsn, committed)` — no filter
      parameter exists. The sparse (SPLADE) stream (hybrid.rs:372-390) is
      equally unfiltered.

      The `FieldFilter` type DOES exist (ft_text_search.rs:948) and
      `TextIndex::search_tag` works correctly (text/store.rs:1026; numeric:
      search_numeric_range, store.rs:1205), but it is only reachable via
      `pre_parse_field_filter` (ft_text_search.rs:1149) — a TAG-only
      FT.SEARCH path that completely bypasses HYBRID.

  (a2) MULTI-SHARD PATH — A SECOND UNFILTERED EXECUTOR (deep-dive finding,
      missed by the first draft): the ft.rs dispatcher ALWAYS routes HYBRID
      through `scatter_hybrid_search` (ft.rs:128-137). For num_shards == 1 it
      short-circuits into `execute_hybrid_search_local` (scatter_hybrid.rs:
      70-96); for num_shards > 1 it runs DFS → per-shard raw-streams fan-out
      via `hybrid_multi.rs::execute_hybrid_search_local_raw_streams`
      (scatter_hybrid.rs:270,292) → coordinator-side `rrf_fuse_three` on the
      stream unions. The cross-shard request is the in-process
      `FtHybridPayload` (src/shard/dispatch.rs:222-240: index_name,
      query_terms, dense/sparse fields+blobs, weights, k_per_stream, top_k,
      global_df, global_n, as_of_lsn, reply_tx) — NO filter field. A filter
      fix that only touches execute_hybrid_search_local is silently bypassed
      on every multi-shard deployment.

  (b) WHY `compose_query_with_filter` IS A NO-OP ON BOTH BRANCHES:
      Lunaris builds `"(@source:{val}) text_query"` (fusion.rs:262) and passes it
      as the `text_query` argument to `hybrid_search`. On arrival at the Moon
      server, the HYBRID dispatcher (ft.rs:69-137: parse_hybrid_modifier at
      ft.rs:72, HybridQuery built at ft.rs:101-112) hands the raw text_query
      to `parse_text_query` inside execute_hybrid_search_local
      (hybrid.rs:332-342). That parser routes `@field:` syntax through the
      TEXT-fields resolver, which errors with `"ERR unknown field 'source'"`
      because `source` is declared as TAG
      (crates/lunaris-storage-moon/src/client.rs:376-380), not as TEXT.
      Composite queries like `"(@source:{val}) text_query"` start with `(`,
      not `@`, so they do NOT trigger `pre_parse_field_filter`
      (ft_text_search.rs:1149-1150:
      `if !query.starts_with(b"@") { return Ok(None); }`), and the
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

  CHANGE A — HybridQuery gains an optional filter field carrying the FULL tree
    (v1 scope upgraded at freeze — Tin Dang 2026-06-12, "Require full filter
    tree in v1"):
    pub filter: Option<HybridFilter>
    where HybridFilter is a new recursive enum in hybrid.rs:
      pub enum HybridFilter {
          Tag { field: String, value: String },          // exact + prefix ({v*})
          Numeric { field: String, min: f64, max: f64 },
          And(Vec<HybridFilter>),
          Or(Vec<HybridFilter>),
      }
    (leaf evaluation reuses the existing FieldFilter machinery,
    ft_text_search.rs:916-923 — no leaf logic is duplicated)

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
    Tree evaluation (full-tree v1): leaves (Tag / Numeric) produce bitmaps via
    TextIndex::search_tag / search_numeric_range; And = bitmap intersection;
    Or = bitmap union; evaluated bottom-up recursively. Depth/width are bounded
    by the wire parser (CHANGE E): max depth 4, max 16 leaves — exceeding either
    returns Frame::Error("ERR FILTER too complex").

  CHANGE C — k_per_stream fan-out when filter is present:
    In effective_k_per_stream() (hybrid.rs:78), when query.filter.is_some(),
    return max(k_per_stream_default, 5 * top_k, 60) instead of 3x, to compensate
    for expected filter attrition on the raw candidate window (k-starvation Must).
    The 5x multiplier is the default; callers may override via explicit K_PER_STREAM.

  CHANGE D — HybridQuery construction in ft.rs:100-111 plumbs the filter field
    (initially None until wire protocol is extended — CHANGE E below):
    hq = HybridQuery { ..., filter: None };

FILE: vendor/moon/src/command/vector_search/hybrid.rs (parse_hybrid_modifier)
      + vendor/moon/src/server/conn/handler_monoio/ft.rs (dispatcher)
      [LOCATION CORRECTED 2026-06-12 deep-dive: HYBRID-clause parsing lives in
       hybrid.rs::parse_hybrid_modifier (returns HybridQueryPartial + consumed
       index); the dispatcher (ft.rs:69-137) then scans LIMIT etc. via
       parse_limit_clause. FILTER scanning slots in alongside — NOT in
       ft_search/parse.rs as first-drafted.]

  CHANGE E — New FT.SEARCH wire modifier FILTER parsed during HYBRID dispatch
    (ft.rs:69-137). Recursive prefix encoding (arity-counted, unambiguous,
    zero lookahead) appended after FUSION RRF [...]:
      FILTER <expr>
      <expr> := TAG @<field> <value>
              | NUMERIC @<field> <min> <max>
              | AND <n> <expr>{n}
              | OR  <n> <expr>{n}
    Examples:
      FILTER TAG @source scratchpad
      FILTER AND 2 TAG @source a TAG @chunk_type fact
      FILTER OR 2 TAG @source a AND 2 TAG @source b NUMERIC @valid_from 0 99
    Limits enforced at parse: depth <= 4, total leaves <= 16 ->
    Frame::Error("ERR FILTER too complex"). Unknown <expr> heads return
    Frame::Error("ERR unsupported FILTER type").
    When FILTER is absent, HybridQuery.filter = None (backward compatible).

FILE: vendor/moon/src/shard/scatter_hybrid.rs
      + vendor/moon/src/shard/dispatch.rs
      + vendor/moon/src/command/vector_search/hybrid_multi.rs
      [REWRITTEN 2026-06-12 deep-dive: the first draft's "no signature change
       needed" was WRONG — the multi-shard path does NOT go through
       execute_hybrid_search_local at all.]

  CHANGE F — thread the filter through BOTH scatter paths:
    (F1) Single-shard fast path (scatter_hybrid.rs:70-96): carried inside
         HybridQuery (CHANGE A) into execute_hybrid_search_local — covered by
         CHANGE B, nothing extra.
    (F2) Multi-shard raw-streams path (num_shards > 1):
         - FtHybridPayload (src/shard/dispatch.rs:222-240) gains
           `pub filter: Option<HybridFilter>`; scatter_hybrid_search copies it
           from HybridQuery when building each per-shard payload.
         - hybrid_multi.rs::execute_hybrid_search_local_raw_streams (the
           per-shard executor, called at scatter_hybrid.rs:270,292) computes
           the SAME allowlist as CHANGE B from ITS OWN shard-local text_index
           (doc_ids are shard-local, so the allowlist cannot be computed at
           the coordinator) and applies it to all three raw streams (BM25 /
           dense / sparse) BEFORE returning them. Filtering must happen
           per-shard pre-return, NOT after coordinator rrf_fuse_three —
           post-fusion filtering would reintroduce k-starvation (filtered-out
           hits would have consumed fused-window slots).
         - The CHANGE C k_per_stream fan-out applies identically on the
           raw-streams executor (same effective_k_per_stream override).

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
        filter: Option<HybridFilter>,   // new — None is fully backward compat
    )
    where HybridFilter is the SDK mirror of the server enum (full tree):
      pub enum HybridFilter {
          Tag { field: String, value: String },
          Numeric { field: String, min: f64, max: f64 },
          And(Vec<HybridFilter>),
          Or(Vec<HybridFilter>),
      }
    Wire encoding: recursive prefix form per CHANGE E (`FILTER TAG @{f} {v}`,
    `FILTER AND {n} ...`), appended after the FUSION RRF clause. When None,
    wire is unchanged (backward compat). The encoder enforces the same
    depth/leaf limits client-side and returns an SDK error before sending.

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

  CHANGE I — new private function filter_to_moon_hybrid_filter (FULL tree, v1):
    fn filter_to_moon_hybrid_filter(f: Option<&Filter>) -> Option<moondb::HybridFilter>
    Translates the COMPLETE Lunaris Filter tree to moondb::HybridFilter:
      Filter::Eq { field, value }          -> HybridFilter::Tag { field, value }
      Filter::StartsWith { field, prefix } -> HybridFilter::Tag { field,
                                               value: prefix (Moon TAG prefix match
                                               via {prefix*} wire encoding) }
      Filter::And(children)                -> HybridFilter::And(translate each)
      Filter::Or(children)                 -> HybridFilter::Or(translate each)
      Filter::ValidTimeRange { from, to }  -> HybridFilter::And([
                                               Numeric{valid_from, ..}, Numeric{valid_to, ..}])
                                               (requires CHANGE L numeric fields)
    Whole-tree-or-nothing rule: if ANY node is untranslatable (future variant),
    the WHOLE tree translates to None and the caller-side post-filter +
    CHANGE C fan-out carry correctness for that query — partial translation is
    FORBIDDEN (an Or with a dropped disjunct returns a SUBSET = silent loss;
    an And with a dropped conjunct returns a superset that post-filter must
    then correctness-trim, which the freeze decision rejects). For every
    variant named in §1 Must, translation is total — post-filter is pure
    defense-in-depth there (per §1 After).

  CHANGE J — compose_query_with_filter is no longer called from fuse_via_moon_native.
    Verify no other callers in the crate; if no callers remain, mark deprecated
    with a doc comment pointing to filter_to_moon_hybrid_filter as the replacement.

FILE: crates/lunaris-storage-moon/src/vector.rs

  CHANGE K — re-validate filter_to_moon TAG rendering (@source:{value}):
    Now that TAG filtering is real server-side (via FT.SEARCH TAG-only path),
    verify the ft_tag_escape formatting used by filter_to_moon matches what Moon's
    TAG parser expects. The HYBRID path uses HybridFilter directly (not the
    text-query encoding), so this is a consistency check, not a logic change.

FILE: crates/lunaris-storage-moon/src/client.rs

  CHANGE L — FT.CREATE schema gains NUMERIC fields valid_from + valid_to
    (client.rs:376-380, alongside the existing SchemaField::Tag("source")) so
    Filter::ValidTimeRange is server-translatable (CHANGE I). MIGRATION CAVEAT:
    existing indexes were created without these fields — new deployments get
    them at scope-init; existing scopes need an index rebuild (document the
    operator recipe; if Moon lacks FT.ALTER, the recipe is drop+recreate+
    re-HSET, which Moon's synchronous inline indexing makes safe). The
    conformance test must cover BOTH a fresh index (numeric fields present)
    and the legacy-index fallback (ValidTimeRange -> whole-tree None ->
    post-filter correctness path).

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
    compound filter scenarios resolved SERVER-SIDE (full-tree v1): asserts the
    §2 And/Or scenarios hold with the Lunaris post-filter DISABLED in the test
    (proving the server translation is correctness-complete, not post-filter-
    masked), plus the whole-tree-or-nothing fallback case.
  - vendor/moon/tests/hybrid_filter_tag.rs (new, Moon-side) — unit test of
    execute_hybrid_search_local with filter=Some(HybridFilter::Tag{...}); asserts
    foreign-source vectors are absent from fused output from BOTH BM25 and KNN.
  - vendor/moon/tests/hybrid_filter_multishard.rs (new, Moon-side; ADDED by the
    2026-06-12 deep-dive) — the F2 discriminator: num_shards > 1, mixed-source
    docs spread across shards, filtered HYBRID query; asserts no foreign-source
    hit in the coordinator-fused output AND that per-shard raw streams were
    filtered pre-fusion (k-starvation: an all-matching corpus still fills
    top_k). Without this test a single-shard-only fix passes everything else.
  - vendor/moon/tests/hybrid_filter_backward_compat.rs (new, Moon-side) —
    filter=None produces identical output to pre-CHANGE-A hybrid_search call.
  - /crates/lunaris-mcp/tests/server_boot.rs — existing server-boot roster test
    remains green (no new MCP tools in this task).
  - Existing per-caller guard tests (WorkingMemory::find, recall.rs) remain
    green and are NOT removed.

────────────────────────────────────────────────────────────────────────────────
Explicitly OUT of scope (named, not silent — 2026-06-12 deep-dive finding):
────────────────────────────────────────────────────────────────────────────────
  FT.NAVIGATE has the SAME hole and is NOT fixed here: Moon's navigate.rs
  (src/command/vector_search/navigate.rs) has zero filter handling, the
  Lunaris port method `vector_navigate` carries no filter, and the Navigate
  operator (crates/lunaris-retrieve/src/operators/navigate.rs:139-145)
  IGNORES ctx.query.filter on its native path — the filter only reaches the
  capability-gated `fallback_vector` (navigate.rs:109-122), where the vector
  TAG rendering is equally dead. A `.filter()`'d Navigate-preset recall on
  Moon silently ignores the filter end-to-end. Recorded as split task
  `ft-navigate-filter-gap` (record-verbatim + split, same pattern as this
  task's own origin). Interim guard candidate for that task: Navigate +
  Some(filter) should degrade to fallback_vector-with-post-filter rather
  than silently ignoring.

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

Status: FROZEN @ v1.1 — v1 approved by Tin Dang 2026-06-12 at the bundle
decision point, choosing "Require full filter tree in v1" over the drafted
first-leaf-only shortcut: the recursive HybridFilter tree (Tag / Numeric /
And / Or), the compound FILTER wire encoding (CHANGE E), the total CHANGE I
translation with the whole-tree-or-nothing rule, and CHANGE L numeric fields
are all v1 scope. No correctness-critical post-filter remains for any §1 Must
variant.
v1.1 amendment (same day, requested by Tin Dang: "deepdive into ../moon then
continue draft correct issues ... from latest codebase"): evidence line
citations corrected against moon HEAD 16bc859 (hybrid files byte-identical
to vendor v0.3.0); CHANGE E relocated to parse_hybrid_modifier/dispatcher;
CHANGE F REWRITTEN for the multi-shard raw-streams path (FtHybridPayload +
hybrid_multi.rs per-shard allowlist — the first draft's "no change needed"
would have shipped a fix silently bypassed on every multi-shard deployment);
hybrid_filter_multishard.rs added as the F2 discriminator; FT.NAVIGATE gap
named explicitly out-of-scope -> split task ft-navigate-filter-gap. The
approved SHAPE (full tree, both branches, k-starvation, no
correctness-critical post-filter) is unchanged — the amendment makes the
deliverables actually satisfy it on the real codebase.

Least-sure flag surfaced at freeze:
  ⚠ [contract] CHANGE L index migration — existing scopes' FT indexes lack the
    valid_from/valid_to NUMERIC fields, and Moon may not implement FT.ALTER;
    the drop+recreate+re-HSET recipe is untested at scale and, mid-rebuild,
    queries see a partial index. Cost if wrong: ValidTimeRange filters on
    legacy scopes silently ride the fallback (whole-tree None + post-filter)
    until reindexed — correct but slower; the conformance test pins both paths.
  ⚠ [contract] the recursive FILTER wire parser (CHANGE E) is the largest new
    Moon-server surface in the task; prefix/arity encoding is unambiguous but
    hand-rolled parsers in handler_monoio have no fuzz coverage today. Cost if
    wrong: malformed FILTER could panic the handler — the Moon PR must include
    parser unit tests incl. truncation/overflow cases before the SHA bump.

<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1-2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: behavioral — every §2 Must has a discriminating live-Moon test
that is RED today for the documented reason (foreign source leaks through the
unfiltered dense-KNN branch) and GREEN after the cross-repo fix. Both §2 Reject
scenarios + the k-starvation guard are pinned. (No line-coverage % target: this
is a black-box behavioral suite over a storage boundary.)

Tests live in: `/crates/lunaris-retrieve/tests/hybrid_filter.rs` `hybrid_filter_k_starvation.rs` `hybrid_filter_and_or.rs` · shared harness `hybrid_filter_common` `mod.rs` · Moon-side staged in `./tests/moon-side`
<!-- Lunaris-side suite is runnable NOW (skips if MOON_URL unreachable). Moon-side
     trio is staged for the pilotspace/moon PR (sequencing step 1) — see
     ./tests/moon-side/README.md. -->

Plan — one test per scenario (assert observable behavior, never internals):
<test_plan>
  Lunaris-side (crates/lunaris-retrieve/tests/, black-box through the DSL .filter()):
  - hybrid_filter::filtered_hybrid_recall_excludes_foreign_source  → Must 1 (Eq).
    PRODUCTION-WIRED: both sources via ingest_episode_with_receipt. RED.
  - hybrid_filter::startswith_filter_constrains_both_branches       → Must 2b (StartsWith). RED.
  - hybrid_filter::correctness_holds_without_any_per_caller_postfilter → Reject 1
    (DSL boundary has no post-filter; correctness must be server-side). RED.
  - hybrid_filter::no_filter_baseline_returns_both_sources          → Reject 2
    (CHANGE A backward-compat). GREEN now+after (invariant guard).
  - hybrid_filter_and_or::and_filter_source_and_valid_time_constrains_both → Must 2c
    (And, source∩valid_time — see GAP 1). RED.
  - hybrid_filter_and_or::or_filter_accepts_either_rejects_third    → Must 2d (Or). RED.
  - hybrid_filter_and_or::valid_time_range_bounds_both_branches     → Must 2e (ValidTimeRange). RED.
  - hybrid_filter_k_starvation::filtered_top_k_is_not_starved       → Must 3
    (CHANGE C guard). GREEN now+after; RED only under a naive starving fix.

  Moon-side (staged → pilotspace/moon tests/, against the new FILTER wire clause):
  - hybrid_filter_tag::hybrid_filter_tag_excludes_foreign_from_both_branches (CHANGE A/B/E)
  - hybrid_filter_multishard::hybrid_filter_multishard_no_foreign_and_no_starvation (CHANGE F2 — F2 discriminator)
  - hybrid_filter_backward_compat::hybrid_no_filter_is_backward_compatible (CHANGE A invariant)
</test_plan>

LIVE-RED EVIDENCE (2026-06-12, native arm64 ../moon/target/debug/moon on :6390):
  6 RED for the right reason (leak counts observed: Eq 1, StartsWith 2, Reject-1 3,
  And 6, Or 2, ValidTimeRange 4) + 2 GREEN invariant guards (no_filter_baseline,
  k_starvation). Compile-clean (harness sound, not a broken-harness red). The
  default MOON_URL :6380 on this host is plain Redis 7.4.8 (no FT) → connect
  hard-fails there by design; point MOON_URL at a real Moon.

GAPS FOUND DURING TESTS (frozen-contract tensions — surfaced, not silently coded):
  - GAP 1 — And-leaf field. §2 And used `chunk_type`, which is NOT an indexed FT
    field (chunks schema = `content` TEXT + `valid_time` NUMERIC + `source` TAG +
    `vec`), conflicting with §1 Must "on indexed fields". DECISION (Tin Dang,
    2026-06-12): treat chunk_type as illustrative; the And test intersects the two
    REAL indexed fields source∩valid_time (Tag ∩ Numeric — the stronger
    heterogeneous-leaf test). Scenario clarification, NOT a contract change.
  - GAP 2 — valid_time not ingest-populated. `valid_time` is declared NUMERIC but
    the production ingest path never emits `valid_time_ms` (pipeline.rs:328-334),
    and CHANGE L does not add it. The And/ValidTimeRange tests therefore
    DIRECT-WRITE chunks carrying valid_time_ms (exercises the Moon/SDK numeric
    capability this task owns, not the ingest→valid_time wiring). Recorded as an
    out-of-scope follow-up in §7 (a Moon-filter fix without ingest population would
    leave DSL ValidTimeRange filters inert on ingested chunks).

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): the filter allowlist is applied AFTER the three
streams are collected and BEFORE rrf_fuse_three — never inside HNSW traversal or
BM25 scoring; per-shard (not coordinator) on the multishard path, so k-starvation
is not reintroduced. Conservative-drop: a dense/sparse hit whose key_hash has no
text doc_id is dropped (cannot confirm it matches an indexed-field filter).

### Step 1 — Moon-side (CHANGE A–G) — DONE + VERIFIED GREEN (2026-06-12)
Implemented via senior-rust-engineer against BUILD-CONTEXT.md (CHANGE A `HybridFilter`
enum in new `hybrid_filter.rs`; B+C pre-RRF allowlist + 5× k fan-out; D+E
recursive panic-free FILTER wire parser + dispatcher plumbing; F multishard
`FtHybridPayload.filter` + per-shard pre-fusion filtering; G SDK param).

VERIFICATION BASE: the original ../moon HEAD (feat/shardslice-migration) is
mid-migration and UNBUILDABLE for tests (33 pre-existing lib errors), so the fix
was cherry-picked onto a clean branch off **tag v0.3.0** (c72c5861 — the base
vendor/moon pins; deep-dive proved hybrid path byte-identical; builds + in-process
harness works). Cherry-pick surfaced 6 v0.3.0-specific wiring points the
migration-authored CHANGE D/F missed (FtHybridPayload field; 2 scatter_hybrid +
1 spsc raw-streams call args; per-shard payload literal; 2 runtime handlers vs
the migration's collapsed one) — fixed in commit c3aa41f.

EVIDENCE (cargo test, off v0.3.0):
  RED  (tests only, no impl): hybrid_filter_multishard leaked foreign-* hits;
       hybrid_filter_backward_compat passed.
  GREEN (impl applied): hybrid_filter_tag + hybrid_filter_multishard +
       hybrid_filter_backward_compat → 3 passed.
  REGRESSION: lib hybrid unit tests (incl. CHANGE E truncation/garbage parser
       tests) + lunaris_hybrid_ft_search → 96 passed, 0 failed.

Branch: `feat/hybrid-filter-pushdown` (worktree /Volumes/Games/tindang-repo/moon-verify).
**PUSHED + PR open: pilotspace/moon#174** (base `main`, MERGEABLE) — rebased onto
origin/main (21 commits ahead of v0.3.0, clean) and RE-VERIFIED green there:
3 conformance + 96 regression = **99 passed, 0 failed** on top of main.
NEXT (blocked on review/merge): merge #174 → bump vendor/moon submodule to the
merged SHA (must be pushed first — never pin an unpushed ref).

### Step 3 — Lunaris-side (CHANGE H/I/J/K) — PENDING submodule bump
Blocked on step 2 (the new moondb SDK with the `filter` param must be vendored
before fusion.rs can call it). Then: remove compose_query_with_filter from
fuse_via_moon_native, add filter_to_moon_hybrid_filter, flip the 6 RED
Lunaris-side tests to GREEN.

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

Watch (reuse scenarios as monitors): per-filter foreign-source leak rate (must be
0 post-fix); filtered-vs-unfiltered recall@k delta (k-starvation regression).
Spec delta for the next loop:
  - FOLLOW-UP (GAP 2, out-of-scope here): the production ingest path does not emit
    `valid_time_ms` (pipeline.rs:328-334), so a DSL `.filter(ValidTimeRange)` on
    ingested chunks stays inert even after this Moon-side fix. A separate task must
    add valid_time to chunk metadata + the write path, then flip the And/ValidTime
    tests from direct-write to ingest-wired. Sibling to the already-split
    `ft-navigate-filter-gap`.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
