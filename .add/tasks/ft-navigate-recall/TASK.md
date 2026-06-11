# TASK: FT.NAVIGATE graph-expanded recall path in lunaris-retrieve

slug: ft-navigate-recall · created: 2026-06-11 · stage: production
phase: done   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: FT.NAVIGATE graph-expanded recall — KNN seeds → server-side BFS over the per-scope graph → re-ranked hits with hop metadata, exposed through the retrieval DSL; plus the write-side `_key` linkage that makes it live on Lunaris data.

Pre-spec evidence (live probes on Moon v0.3.0 @ 6390, 2026-06-11):
  - END-TO-END PROVEN: `FT.NAVIGATE idx "*=>[KNN k @vec $v]" PARAMS 2 v <blob> HOPS n [HOP_PENALTY p] [DECAY λ]` surfaces graph-linked far docs (`b@hop1 final=0.1, c@hop2 final=0.2` displacing the weak KNN tail). Reply = FT.SEARCH shape + `__vec_score`/`__hop_depth`/`__final_score`, lower=better, truncated at K (k parsed from the KNN clause; candidates capped at 3k).
  - LINKAGE RULE: expansion works ONLY over graph nodes carrying a `_key` property = the FT doc's Redis HASH key, and ONLY when written via `GRAPH.ADDNODE` (registers `key_to_node`; vendor/moon graph_write.rs:156-180). Cypher MERGE/CREATE/SET NEVER register (verified in source + live: Cypher-written graph → KNN-only, hop_depth=0). GRAPH.SETPROP is documented but UNIMPLEMENTED on v0.3.0.
  - TXN SAFE: GRAPH.ADDNODE works inside `TXN BEGIN/COMMIT`, has read-your-writes (`MATCH … RETURN id(n)` sees the in-TXN node), and rolls back on ABORT — the linkage can ride the single INGEST-04 atomic_write.
  - SEED IDENTITY: Lunaris graph nodes are ENTITIES (id = 16-byte EntityId) and every entity ALSO gets `VectorUpsert{entities}` under the same id → its FT doc key `{ft_index_name(scope,"entities")}:{hex(id)}` is derivable inside atomic.rs with zero schema change. Chunks are NOT graph nodes → navigate over `chunks` degrades to KNN-only (graceful, server-side).

Framings weighed: write-side ADDNODE+`_key` linkage in atomic.rs + additive `vector_navigate` port method + DSL operator (chosen — only path that makes FT.NAVIGATE live on v0.3.0; probes close every unknown) · upstream Moon change to register `_key` from Cypher writes (cleanest long-term, but cross-repo red/green + re-pin + release inside a 1-task window; deferred as a §7 delta) · client-side emulation (vector_search + graph_traverse + client re-rank: 2+ round trips, re-implements what Moon already ships — anti-goal of this milestone) · read-side only without linkage (ships a dead feature: every production graph returns hop_depth=0).
Scope boundary: `entities` is the navigable index (the only FT index whose docs are graph nodes). SDK (py/ts) exposure deferred to a follow-up task; Postgres/embedded get the NotSupported default + capability false.
Must:
<must>
  - WRITE LINKAGE: `WriteOp::GraphNode` on Moon writes via existence-check (`MATCH (n:{label}) WHERE n.id='{hex}' RETURN id(n)`) then `GRAPH.ADDNODE` (new node, with `_key` = entities FT doc key + `id`/`id_hex` + existing props) or Cypher SET update (existing node, today's path) — INSIDE the existing single TXN (INGEST-04 intact: still exactly one atomic_write)
  - Node-identity parity: ADDNODE-written nodes carry the same `id`, `id_hex`, `name`, `type`, `confidence` props the MERGE writer produced, so `Graph::anchored`'s Cypher (`MATCH (n {id_hex: …})`) and GraphEdge's `WHERE a.id=…` keep matching
  - READ: additive `StoragePort::vector_navigate(scope, index, query, k, hops, hop_penalty: Option<f64>, decay: Option<&GraphDecay>) -> Vec<NavigateHit>`; default impl NotSupported("graph_navigate_unsupported…"); Moon override = raw FT.NAVIGATE
  - `NavigateHit { id: Vec<u8>, vec_score: f32, hop_depth: u32, final_score: f32 }` — id decoded via the existing `<index>:`-prefix-strip + hex-decode rule; lower final_score = better
  - `StorageCapabilities += #[serde(default)] graph_navigate_native: bool` (Moon true, others false)
  - DSL: `Navigate::new(index, k).with_hops(n)` operator in lunaris-retrieve (+ `.with_decay(GraphDecay)`, `.with_hop_penalty(p)`), embeds via `QueryContext::embed_once`, capability-gated: backend without `graph_navigate_native` degrades to plain `vector_search` semantics (same contract as degraded_fallback — recall never errors because navigation is unavailable)
  - DECAY surface (milestone item "DSL surface for decay + navigate"): `Graph::anchored(...).with_decay(GraphDecay)` rides the existing `graph_traverse_decayed` (graph-decay-recency contract)
  - Recency flip + navigate proven LIVE through the production path: ingest (real `Lunaris::ingest` or `ingest_structured`) → entities written with `_key` → `Navigate` recall surfaces a graph-linked entity that plain Vector recall at the same k does NOT return (Built ≠ wired discriminator)
</must>
Reject:
<reject>
  - hops == 0 or hops > 5 (Moon MAX_EXPAND_DEPTH) -> "navigate_invalid_hops" at operator/type construction, no IO
  - hop_penalty NaN/∞/negative -> "navigate_invalid_hop_penalty" at construction, no IO
  - decay invalid -> already unrepresentable via GraphDecay constructors (graph-decay-recency contract)
  - vector_navigate Some on a default-impl backend -> StorageError::NotSupported("graph_navigate_unsupported: backend has no native navigate") — DSL layer catches this + capability false → vector_search fallback
  - navigate over an index whose docs have no graph nodes (e.g. chunks) -> NOT an error: Moon returns KNN-only hits with hop_depth=0 (server fallback, passthrough)
  - empty embedding / empty index -> existing vector_search error semantics passthrough (no new codes)
</reject>
After:
<after>
  - Production ingest writes navigable entity nodes; pre-existing nodes (written by the MERGE-only writer) stay valid but un-navigable until touched/re-ingested — operator note in docs/migration
  - A recall composed as `Navigate::new("entities", k).with_hops(2)` returns graph-expanded entities on Moon and clean vector hits on SQLite/Postgres — no caller branching
  - bench-rerun-v030 can A/B plain vs navigate recall using only public DSL surface
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ ADDNODE prop typing can represent today's MERGE props faithfully (notably `aliases` array + float `confidence`) — lowest confidence because GRAPH.ADDNODE's prop-value parser tier (string/int/float/array?) is unverified; if wrong: arrays serialize as JSON strings on the ADDNODE path and any Cypher reading `aliases` structurally breaks — mitigation: probe in tests phase BEFORE build; production readers of `aliases` are grep-checked (currently none in Cypher)
  ⚠ The existence-check read (`MATCH … RETURN id(n)`) inside large ingest TXNs doesn't measurably regress ingest p99 (1 extra GRAPH.QUERY per entity per ingest) — if wrong: batch the checks into one `WHERE n.id IN [...]` read per atomic_write; contract unaffected (wire detail)
  - [ ] FT.NAVIGATE respects the per-scope index/graph pairing (find_target_graph scans ALL graphs for seed keys — keys are scope-prefixed so cross-scope alias is impossible: `{ft_index_name(scope,…)}:{hex}` is unique per scope)
  - [ ] entity stub embeddings (det_vec) are good enough seeds for the live discriminator (the test controls embeddings directly, so this only affects demo realism, not the contract)
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: navigate surfaces a graph-linked entity plain vector recall misses (live Moon, production path)
  Given an ingest that produced entities A (vector-near the query), B and C (vector-FAR),
        with graph edges A->B->C written by the production atomic_write
  When recall runs Vector::new("entities", k) and Navigate::new("entities", k).with_hops(2)
  Then the Vector run does NOT contain B
   And the Navigate run contains B with hop_depth >= 1 and a final_score
   And both runs return A

Scenario: write linkage is idempotent (re-ingest dedupes)
  Given the same entity ingested twice (same EntityId)
  When the second atomic_write runs
  Then the graph holds exactly ONE node with that id
  And its props reflect the latest write (SET-update path)

Scenario: navigate on a backend without native support degrades to vector search
  Given SQLite/embedded storage (capabilities().graph_navigate_native == false)
  When Navigate::new("entities", k).with_hops(2) executes
  Then it returns exactly the hits plain vector search returns (no error, no hop metadata)
  And StoragePort::vector_navigate on the default impl returns NotSupported("graph_navigate_unsupported…")

Scenario: invalid navigate parameters are unrepresentable
  Given hops ∈ {0, 6} or hop_penalty ∈ {NaN, ∞, -0.1}
  When the Navigate operator (or NavigateSpec type) is constructed
  Then construction errors "navigate_invalid_hops" / "navigate_invalid_hop_penalty" before any IO

Scenario: navigate over a graph-less index falls back server-side
  Given chunks docs in the FT index with no chunk graph nodes
  When vector_navigate runs on "chunks" with hops 2
  Then hits come back KNN-only with hop_depth == 0 (no error)

Scenario: decay composes with navigate and with Graph::anchored
  Given the seeded entity graph with a stale and a fresh discovery edge
  When Navigate::…​.with_decay(GraphDecay::new(λ)) and Graph::anchored(…).with_decay(…) execute
  Then both run without error; the navigate run's stale-edge hit carries a worse final_score than without decay

Scenario: capability flag reports navigate support
  Given MoonStorage and embedded storage
  When capabilities() is read
  Then Moon reports graph_navigate_native == true, embedded false
  And an OLD serialized capabilities payload (without the field) deserializes to false
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
NEW TYPES (crates/lunaris-core/src/storage/types.rs):
  #[derive(Clone, Copy, Debug, PartialEq)]
  pub struct NavigateSpec { hops: u32, hop_penalty: Option<f64>, decay: Option<GraphDecay> }  // fields PRIVATE
    NavigateSpec::new(hops: u32) -> Result<Self, StorageError>      // 1..=5, else Backend("navigate_invalid_hops: …")
    .with_hop_penalty(p: f64)    -> Result<Self, StorageError>      // finite && >= 0, else Backend("navigate_invalid_hop_penalty: …")
    .with_decay(d: GraphDecay)   -> Self                            // already-validated type, infallible
    .hops() -> u32 · .hop_penalty() -> Option<f64> · .decay() -> Option<GraphDecay>

  #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
  pub struct NavigateHit { pub id: Vec<u8>, pub vec_score: f32, pub hop_depth: u32, pub final_score: f32 }
    // id = decoded doc id (prefix-stripped + hex-decoded, same rule as VectorHit); lower final_score = better

PORT (crates/lunaris-core/src/storage/port.rs) — ADDITIVE, default impl:
  async fn vector_navigate(&self, scope: &Scope, index: &str, query: &[f32], k: usize,
                           spec: &NavigateSpec) -> Result<Vec<NavigateHit>, StorageError>
    default: Err(StorageError::NotSupported("graph_navigate_unsupported: backend has no native navigate"))

CAPABILITIES: + #[serde(default)] pub graph_navigate_native: bool   // Moon true · Postgres/embedded false

MOON WIRE (crates/lunaris-storage-moon/src/vector.rs or navigate.rs) — override, raw RESP:
  FT.NAVIGATE {ft_index_name(scope,index)} "*=>[KNN {k} @vec $v]" PARAMS 2 v <f32-le blob>
              HOPS {hops} [HOP_PENALTY {p}] [DECAY {λ}]
  reply rows: doc_key + [__vec_score, __hop_depth, __final_score] -> NavigateHit (decode_key on doc_key)
  NOTE: FT.NAVIGATE DECAY has no time-weight slot (server fixes time_weight=1.0) — a NavigateSpec whose
        GraphDecay carries time_weight Some(w) sends only λ; w is documented as GRAPH.QUERY-only.

WRITE LINKAGE (crates/lunaris-storage-moon/src/atomic.rs, WriteOp::GraphNode arm — same TXN, INGEST-04 intact):
  1. GRAPH.QUERY <scope_graph> "MATCH (n:{label}) WHERE n.id='{id_hex}' RETURN id(n)"   (read-your-writes in TXN: proven)
  2a. absent  -> GRAPH.ADDNODE <scope_graph> {label} id {id_hex} id_hex {id_hex} _key {ft_index_name(scope,"entities")}:{id_hex} <props…>
  2b. present -> Cypher SET update (today's MERGE/SET path, unchanged)
  GraphEdge arm: UNCHANGED (WHERE-form MATCH + MERGE)

DSL (crates/lunaris-retrieve/src/operators/navigate.rs):
  Navigate::new(index, k) -> Navigate            // mirrors Vector::new, clamp_k applied
    .with_hops(n) / .with_hop_penalty(p) / .with_decay(d)   // validation via NavigateSpec, errors at execute()
    impl Retriever: embed via QueryContext::embed_once;
      capabilities().graph_navigate_native == true  -> vector_navigate -> RawHit { score = final_score inverted to the
                                                       existing higher-is-better RawHit convention, hop_depth in provenance }
      false (or NotSupported from the port)          -> vector_search fallback, identical RawHit shape, hop_depth absent
  Graph::anchored(...).with_decay(GraphDecay)    // threads into graph_traverse_decayed (existing contract)

Error responses (every §1 Reject has one):
  navigate_invalid_hops        -> StorageError::Backend, at construction, no IO
  navigate_invalid_hop_penalty -> StorageError::Backend, at construction, no IO
  graph_navigate_unsupported   -> StorageError::NotSupported (default port impl; DSL converts to fallback)
  graph-less index             -> Ok(hits with hop_depth=0) — server fallback passthrough, NOT an error

Schema: no persistent schema change. New `_key` + `id` props on NEWLY-written entity graph nodes only;
        migration note docs/migration/0.7-navigate-linkage.md (old nodes un-navigable until re-touched).
```

Status: FROZEN @ v1 — approved by Tin Dang (2026-06-11, freeze #3)
Least-sure flag surfaced at freeze:
  ⚠ [spec] GRAPH.ADDNODE prop typing for `aliases` (array) + `confidence` (float) is unverified — because the ADDNODE prop-value parser tier is undocumented; if wrong: arrays land as JSON strings on the ADDNODE path; cost is fixture/writer-detail churn only (no production Cypher reads `aliases`), resolved by a dedicated probe test BEFORE build.
  ⚠ [spec] one existence-check GRAPH.QUERY per entity inside the ingest TXN may move ingest p99 — if wrong: batch the checks (`WHERE n.id IN [...]`) per atomic_write; wire detail, contract shape unaffected.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every §2 scenario has an executable test; red = compile failure on the missing NavigateSpec/NavigateHit/vector_navigate/Navigate API + live discriminators on Moon v0.3.0.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - core navigate_spec.rs::rejects_invalid_hops / rejects_invalid_hop_penalty: 0/6 hops, NaN/∞/neg penalty error with named codes; valid values construct + accessors echo; with_decay composes
  - core navigate_spec.rs::default_port_navigate_not_supported: mock keeping the default returns NotSupported("graph_navigate_unsupported…")
  - core navigate_spec.rs::capabilities_serde_default: old payload without graph_navigate_native parses to false
  - moon-it navigate_recall.rs::PROBE addnode_prop_typing (resolves §1 ⚠ #1 BEFORE build): ADDNODE with float + array-ish props, read back via Cypher, record actual typing
  - moon-it navigate_recall.rs::navigate_surfaces_graph_linked_entity: production ingest path (Lunaris handle or direct atomic_write with real WriteOps) seeds near/far entities + edges; Vector misses B, Navigate(hops=2) returns B with hop_depth>=1  [DISCRIMINATOR — Built≠wired]
  - moon-it navigate_recall.rs::reingest_is_idempotent: same EntityId twice -> exactly one graph node (count via Cypher), props updated
  - moon-it navigate_recall.rs::navigate_graphless_index_knn_only: chunks index -> hop_depth all 0, no error
  - moon-it navigate_recall.rs::navigate_decay_composes: with_decay(λ) executes; stale-edge hit's final_score worsens vs no-decay run
  - moon-it navigate_recall.rs::moon_reports_graph_navigate_native: capability true
  - retrieve (mock) navigate_fallback.rs::navigate_degrades_to_vector_on_unsupported: mock port with graph_navigate_native=false -> Navigate returns the vector_search hits, no error
  - retrieve (mock) navigate_fallback.rs::anchored_with_decay_threads_through: Graph::anchored(...).with_decay(d) calls graph_traverse_decayed with Some(d) (recorded by mock)
</test_plan>

Tests live in: `crates/lunaris-core/tests/navigate_spec.rs` · `crates/lunaris-storage-moon/tests/navigate_recall.rs` · `crates/lunaris-retrieve/tests/navigate_fallback.rs` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): the `_key` linkage and ALL node writes stay inside the single `atomic_write` TXN (INGEST-04); navigate parameters reach the wire only through a validated `NavigateSpec`; the `Navigate` operator must NEVER fail a recall because navigation is unavailable (capability gate + NotSupported fallback).
Code lives in: `crates/lunaris-core/src/storage/{types,port,capabilities}.rs` + re-exports · `crates/lunaris-storage-moon/src/{navigate.rs (new), atomic.rs, lib.rs, vector.rs (decode_key pub(crate)), graph.rs (parse_graph_reply pub(crate))}` · `crates/lunaris-retrieve/src/operators/{navigate.rs (new), graph.rs, mod.rs}` + lib.rs · `docs/migration/0.7-navigate-linkage.md` · ~40 capability literals (mechanical `graph_navigate_native` insertion)
Constraints: do NOT change any test or the contract; allow-list packages only (zero new deps); ask if unclear.
Build notes (deviations, all wire-level, contract shape untouched):
  - ADDNODE carries ONLY `_key` inline; `id` + all other props flow through one Cypher `SET … WHERE id(n) = <nid>` right after. Reason: ADDNODE's prop parser auto-coerces digit-only strings (live probe: an all-digit hex id stored as Float made `WHERE n.id='…'` return 0 rows). §3 wrote "ADDNODE … + id/id_hex + props" — the OBSERVABLE contract (node carries identical props, `_key` registered) holds; the prop transport moved to SET for type fidelity.
  - Update path uses `MATCH … WHERE n.id = '…' SET …` (WHERE form) instead of the old `MERGE (n {id: …})` — same observable behavior, avoids Moon's known inline-property-filter quirk entirely.
  - Test fixture DIM corrected 8 → 768 mid-build: the Phase-22 dim guardrail rejected the 8-d handle against the shared server's 768-d indices and the graceful-skip silently no-opped the suite (caught by timing + --nocapture; suite re-observed GREEN for real at 768).

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — core `navigate_spec` 5/5 · retrieve `navigate_fallback` 5/5 (mock gating + anchored-decay threading) · moon-it `navigate_recall` 6/6 LIVE on Moon v0.3.0 @ 6390 (0.46s real run, ADDNODE_PROP_TYPING VERDICT printed; the earlier 0.01s "pass" was a silent dim-guardrail SKIP, caught and fixed by setting the fixture to 768-d) · lunaris-storage-moon lib 67/67 · lunaris-retrieve lib 85/85 · full workspace green except two PRE-EXISTING environment failures: `tree_recall` (stale Moon v0.2 on 6380 — passes 3/3 vs 6390) and `dim_configurable` (cross-suite index pollution: the suite itself creates global 1536-d indices, mutually exclusive with 768-d suites on one shared server — root-caused this session, recorded as delta)
- [x] coverage did not decrease — 16 new tests + 5 new in-module unit tests (navigate.rs parsers, operator builders); zero removed/weakened
- [x] no test or contract was altered during build — exceptions recorded honestly in §5: fixture DIM 8→768 (un-skipping the suite, assertions untouched) + `cargo fmt` whitespace; contract §3 deviations are wire-transport only (ADDNODE+SET split), observable shape intact
- [x] concurrency / timing safe — all new IO is sequential on the single TXN connection (`typed`/`inner_mut` borrows, no locks, nothing held across `.await`); the existence-check relies on Moon TXN read-your-writes (probe-verified) so duplicate GraphNode ops within one batch stay idempotent
- [x] no exposed secrets, injection openings, or unexpected dependencies — `_key`/ADDNODE values go as distinct RESP args (no Cypher interpolation); the SET path reuses the existing `build_set_clause` escaping; `id(n) = {nid}` interpolates an i64 returned by Moon, not caller input; zero new crates
- [x] layering & dependencies follow CONVENTIONS.md — types in lunaris-core, wire in lunaris-storage-moon (keyspace helpers from lunaris_core re-exports), DSL in lunaris-retrieve; additive port method = third use of the `queue_depth` precedent; raw RESP only where the typed SDK lacks the surface (FT.NAVIGATE DECAY slot, ADDNODE)
- [x] reviewed — auto-resolved under `autonomy: auto` (run.md); no security/concurrency/architecture residue found

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `NavigateSpec`/`NavigateHit` exported core→root, consumed by port + Moon + DSL; `vector_navigate` Moon override exercised live by 4 moon-it tests; `Navigate` operator exported from lunaris-retrieve and exercised through `Retriever::retrieve` with the real `QueryContext`; the WRITE linkage is wired into THE production `atomic_write` (the moon-it suite seeds exclusively through it — Built ≠ wired discriminator green: navigate surfaces B@hop≥1 that Vector misses)
- [x] DEAD-CODE (code) — clippy `--all-targets` 0 warnings with `unreachable_pub` deny; `parse_ft_navigate`/`decode_key`/`parse_graph_reply` all referenced; no orphan
- [x] SEMANTIC (prose) — `docs/migration/0.7-navigate-linkage.md` re-read against probe evidence: old-node un-navigability, KNN-only degradation, `_key` reservation, in-place-backfill impossibility all stated and probe-backed

### GATE RECORD
Outcome: PASS (auto-resolved — complete evidence, no escalating residue)
Reviewed by: Claude (ADD auto-gate, autonomy: auto) · date: 2026-06-11

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): share of navigate recalls returning any hop_depth ≥ 1 hit (linkage health — 0% on an old scope means un-migrated nodes); ingest p99 delta from the per-entity existence check (freeze flag #2 — batch via `WHERE n.id IN […]` if it moves); rate of `navigate_invalid_*` construction errors from SDK consumers.
Spec delta for the next loop (bench-rerun-v030): A/B plain `Vector("entities", k)` vs `Navigate("entities", k).with_hops(2)` is now expressible in public DSL only; also measure ingest-side cost of the existence-check read at 10k-entity scale.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · folded] the model missed multi-tenancy (evidence: scenario_x failed) -->
- [SDD · folded] probe-before-freeze paid for itself twice: the ADDNODE prop-coercion hazard (digit-hex → Float) and the GRAPH.SETPROP docs-drift (documented, unimplemented) were both caught by live probes BEFORE the contract was built against them (evidence: /tmp probes 2026-06-11; §5 build notes).
- [ADD · folded] graceful-skip false-pass struck AGAIN inside this very task (dim-guardrail SKIP read as 6/6 in 0.01s) — the open delta from graph-decay-recency is now twice-evidenced; the fix candidate is a `MOON_IT_REQUIRED=1` env that turns connect_or_skip failures into hard failures in CI (evidence: --nocapture run showing 6× SKIP).
- [DDD · folded] dim_configurable and 768-d moon-it suites are mutually exclusive on one shared server (the suite itself creates sticky global 1536-d indices) — root cause of the task-1 "cross-suite pollution" mystery; suites need per-run server isolation or per-suite index prefixes (evidence: fresh-server 6391 sequence: dim 3/3 real, then navigate 6× SKIP on "facts is 1536-d").
- [SDD · folded] upstream Moon enhancement candidate: register `_key` from Cypher write paths (MERGE/CREATE/SET) so existing graphs can be backfilled in place and the ADDNODE+SET two-step collapses back to one MERGE (evidence: vendor/moon graph_write.rs:156-180 is the only registration site; docs/migration/0.7-navigate-linkage.md operator note).
- [TDD · folded] the "production-path discriminator" pattern (seed ONLY through atomic_write, assert the feature observable end-to-end) caught nothing this time precisely because the build threaded the linkage through the real writer first — keep it as the default fixture style for storage-visible features (evidence: navigate_surfaces_graph_linked_entity green on first real run).
