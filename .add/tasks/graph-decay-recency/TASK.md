# TASK: Decay λ through graph_traverse (GRAPH.QUERY --decay)

slug: graph-decay-recency · created: 2026-06-11 · stage: production
phase: done   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Recency-decay graph traversal — thread λ through the storage port to Moon's `GRAPH.QUERY ... --decay <λ> [--time-weight <w>]`
Framings weighed: additive port method `graph_traverse_decayed` + capability flag (chosen — mirrors the `queue_depth` additive precedent, zero churn on existing impls/mocks) · change `graph_traverse` signature (breaks 10+ impl/mock sites) · encode decay inside CypherQuery (smuggles a transport flag into the query type, leaks into PG/AGE which has no decay)
Scope boundary: storage layer ONLY. The retrieval-DSL / SDK exposure of decay belongs to `ft-navigate-recall` per the milestone's shared-contract ownership ("Retrieval DSL surface for decay + navigate").
Must:
<must>
  - `lunaris_core::storage::types::GraphDecay` validated type: `new(lambda)` accepts finite λ ≥ 0 only; `with_time_weight(w)` accepts finite w > 0 only — invalid values are unrepresentable (constructor errors, mirroring Moon's strict server-side validation)
  - Additive `StoragePort::graph_traverse_decayed(scope, query, as_of, decay: Option<&GraphDecay>)` with a default impl: `None` delegates to `graph_traverse` byte-for-byte; `Some` returns NotSupported — existing backends/mocks compile unchanged
  - Moon override appends `--decay <λ>` (and `--time-weight <w>` when set) to GRAPH.QUERY; composes with `--params` AND with `VALID_AT` (decay + as_of in one query)
  - `StorageCapabilities` gains `#[serde(default)] graph_decay_native: bool` — true on Moon, false on Postgres/embedded
  - Recency flip proven live: a shortestPath whose cheapest-by-weight route uses OLDER edges flips to the fresher route under λ — observable in the returned path row
  - Decay None path produces results identical to plain `graph_traverse` (delegation guarantee)
</must>
Reject:
<reject>
  - λ that is NaN / infinite / negative -> constructor error "graph_decay_invalid_lambda" (never reaches the wire)
  - time_weight that is NaN / infinite / ≤ 0 -> "graph_decay_invalid_time_weight"; time_weight WITHOUT decay is unrepresentable (only constructible via GraphDecay::with_time_weight)
  - decay Some on a backend without native support -> StorageError::NotSupported("graph_decay_unsupported…") from the default impl — callers gate on capabilities().graph_decay_native
  - decay on a write Cypher (CREATE/SET/DELETE/MERGE) -> Moon server rejection surfaced as StorageError::Backend (passthrough; Lunaris does not pre-parse Cypher)
</reject>
After:
<after>
  - Any port-level caller can request recency-biased traversal on Moon with zero behavior change for callers that never pass decay
  - PG/SQLite paths untouched; conformance suite unaffected (new method has a safe default)
  - ft-navigate-recall can build its DSL surface on GraphDecay + the capability flag without re-deciding shapes
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Moon's shortestPath edge cost reads the `weight` property Lunaris writes on GraphEdge ops (docs say cost = |weight| + λ·w·age but the property-name binding is unverified against Lunaris-written edges) — lowest confidence because we have never run shortestPath over Lunaris-written edges with explicit weights; if wrong: the live flip test stays red and the FIXTURE (property name / default-weight assumption) adjusts, not the contract
  ⚠ Second-scale edge-age resolution is reliable in a test (edges stamped from the shard-cached clock; flip needs age(old)−age(fresh) ≈ 3 s to dominate a 0.2 weight delta at λ=2) — if wrong: flaky test; mitigation is a larger sleep + larger λ before declaring red-for-wrong-reason
  - [ ] `--decay` composes with `--params` and `VALID_AT` in one command line (docs list them as independent clauses of the same read path)
  - [ ] Adding a `#[serde(default)]` bool to StorageCapabilities (Copy struct, literal-constructed) only requires updating the 3 backend constructors + conformance fixtures — compile-enforced, no silent drift
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: decay flips shortestPath toward fresher edges (live Moon)
  Given a per-scope graph with route OLD: A-[w=0.4]->B-[w=0.4]->C written first,
        a ~3 s pause, then route FRESH: A-[w=1.0]->C written second
  When graph_traverse_decayed runs shortestPath(A→C) with decay None, then again with λ=2.0
  Then the None run returns the OLD 2-hop route (cheapest by weight, 0.8 < 1.0)
   And the λ=2.0 run returns the FRESH direct route (age penalty dominates)

Scenario: decay None delegates byte-for-byte
  Given the same seeded graph
  When graph_traverse(q) and graph_traverse_decayed(q, decay=None) both run
  Then headers and rows are identical
  And no decay flag appears on the wire for the None path

Scenario: invalid lambda is unrepresentable
  Given λ ∈ {NaN, ∞, -0.5}
  When GraphDecay::new(λ) is called
  Then it errors "graph_decay_invalid_lambda" before any IO
  And the same for with_time_weight(w) with w ∈ {NaN, ∞, 0.0, -1.0} -> "graph_decay_invalid_time_weight"

Scenario: unsupported backend refuses decay
  Given a backend whose StoragePort keeps the default graph_traverse_decayed (capabilities().graph_decay_native == false)
  When graph_traverse_decayed runs with decay Some
  Then it returns StorageError::NotSupported("graph_decay_unsupported…")
  And decay None on the same backend still works (plain delegation)

Scenario: decay composes with VALID_AT and --params (live Moon)
  Given the seeded graph and a wall-clock timestamp after seeding
  When graph_traverse_decayed runs with decay Some(λ=2.0) AND as_of Some(ts) AND a $param in the Cypher
  Then the query executes without error and returns a parseable result
  And the write-Cypher rejection stays server-side (a MERGE with --decay surfaces StorageError::Backend)

Scenario: capability flag reports decay support
  Given MoonStorage and an embedded/SQLite storage
  When capabilities() is read
  Then Moon reports graph_decay_native == true and the others false
  And serde round-trip of an OLD capabilities payload (without the field) defaults it to false
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
NEW TYPE (crates/lunaris-core/src/storage/types.rs):
  #[derive(Clone, Copy, Debug, PartialEq)]
  pub struct GraphDecay { lambda: f64, time_weight: Option<f64> }   // fields PRIVATE — validity by construction
    GraphDecay::new(lambda: f64) -> Result<Self, StorageError>      // finite && >= 0, else Backend("graph_decay_invalid_lambda: …")
    .with_time_weight(w: f64)   -> Result<Self, StorageError>       // finite && > 0,  else Backend("graph_decay_invalid_time_weight: …")
    .lambda() -> f64 · .time_weight() -> Option<f64>                // read accessors

PORT (crates/lunaris-core/src/storage/port.rs) — ADDITIVE method, default impl, no existing impl breaks:
  async fn graph_traverse_decayed(&self, scope: &Scope, query: &CypherQuery, as_of: Option<Hlc>,
                                  decay: Option<&GraphDecay>) -> Result<GraphResult, StorageError>
    default: decay None  -> self.graph_traverse(scope, query, as_of)
             decay Some  -> Err(StorageError::NotSupported("graph_decay_unsupported: backend has no native decay traversal"))

CAPABILITIES (crates/lunaris-core/src/storage/capabilities.rs):
  + #[serde(default)] pub graph_decay_native: bool      // Moon: true · Postgres: false · embedded/SQLite: false

MOON WIRE (crates/lunaris-storage-moon/src/graph.rs) — override:
  GRAPH.QUERY lunaris_{scope}_graph "<cypher>" [--params <json>] --decay <λ> [--time-weight <w>] [VALID_AT <ms>]
  (raw RESP path — the typed SDK GraphClient has no flag slots; mirrors the existing VALID_AT escape hatch)

Error responses (every §1 Reject has one):
  graph_decay_invalid_lambda       -> StorageError::Backend, at construction, no IO
  graph_decay_invalid_time_weight  -> StorageError::Backend, at construction, no IO
  graph_decay_unsupported          -> StorageError::NotSupported, default port impl
  decay-on-write-Cypher            -> StorageError::Backend (Moon server rejection passthrough, verbatim message)

Schema: no persistent schema change; no DSL/SDK surface change (owned by ft-navigate-recall).
```

Status: FROZEN @ v1 — approved by Tin Dang (2026-06-11, freeze #2)
Least-sure flag surfaced at freeze:
  ⚠ [spec] Moon shortestPath edge cost is assumed to read the `weight` property Lunaris writes on GraphEdge ops — because the property-name binding is unverified over Lunaris-written edges; if wrong: the live flip test stays red and the FIXTURE adjusts, contract shape unaffected.
  ⚠ [test] the recency-flip discriminator needs second-scale edge-age resolution (sleep ~3 s, λ=2.0) — because the shard-cached clock's stamping granularity over a test window is unproven; if wrong: flaky test, mitigated by larger sleep/λ.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every §2 scenario has an executable test. Red = compile failure on the missing GraphDecay/graph_traverse_decayed API (the Rust analog of red-for-missing-implementation) for the core suite, plus live discriminators on Moon v0.3.0.
Fixture note: the live graph is seeded via raw GRAPH.ADDNODE/ADDEDGE (WEIGHT arg) mirroring Moon's own DECAY-01/02 fixture in scripts/test-commands.sh:1918-1963 — edge weight is an ADDEDGE argument, NOT a Cypher property, which resolves the freeze's ⚠ [spec] flag: Lunaris-MERGE-written edges get the default weight; weighted-edge writing is out of scope here (raw seeding is test-only and legal — the no-raw rule is queue.rs's contract, not the graph tests').
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - core graph_decay.rs::rejects_invalid_lambda / rejects_invalid_time_weight: NaN/∞/neg (and ≤0 for w) error with the named codes; valid values construct + accessors echo
  - core graph_decay.rs::default_port_decay_some_not_supported: mock keeping the default method returns NotSupported("graph_decay_unsupported…")
  - core graph_decay.rs::default_port_decay_none_delegates: mock's graph_traverse marker result comes back unchanged through graph_traverse_decayed(None)
  - core graph_decay.rs::capabilities_serde_default: serialize → remove graph_decay_native key → deserialize → field defaults false
  - moon-it graph_decay_recency.rs::decay_flips_shortest_path: stale-direct (w=1.0) vs fresh-detour (0.6+0.6, after 2.5s sleep); None → direct (no B); λ=5 → detour (B present)  [DISCRIMINATOR]
  - moon-it graph_decay_recency.rs::decay_none_matches_plain_traverse: identical headers+rows
  - moon-it graph_decay_recency.rs::decay_composes_with_params_and_valid_at: λ=5 + $param + as_of executes, parseable
  - moon-it graph_decay_recency.rs::write_cypher_with_decay_rejected: MERGE + decay → StorageError::Backend (server rejection passthrough)
  - moon-it graph_decay_recency.rs::moon_reports_graph_decay_native: capabilities().graph_decay_native == true
</test_plan>

Tests live in: `crates/lunaris-core/tests/graph_decay.rs` · `crates/lunaris-storage-moon/tests/graph_decay_recency.rs` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): decay parameters reach the wire ONLY through a validated `GraphDecay` (constructor-enforced finite λ ≥ 0 / w > 0) — no raw f64 ever hits GRAPH.QUERY; the None path must remain byte-identical to `graph_traverse`.
Code lives in: `crates/lunaris-core/src/storage/{types,port,capabilities}.rs` · `crates/lunaris-core/src/{lib,storage}.rs` (re-exports) · `crates/lunaris-storage-moon/src/{lib,graph}.rs` · ~40 capability literals (mechanical `graph_decay_native` insertion, compile-enforced)
Constraints: do NOT change any test or the contract; allow-list packages only (zero new deps used); ask if unclear.
Build note: `query_raw_valid_at` was refactored to delegate to a shared `query_raw_with_clauses` builder (clause order: `--params` → `--decay [--time-weight]` → `VALID_AT`) so VALID_AT and decay share one wire path instead of duplicating it. `cargo fmt --all` re-wrapped two import lists and the test file's whitespace — formatting only, no semantic test edits.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — core `graph_decay` 5/5 (0.00s) · moon-it `graph_decay_recency` 5/5 live on Moon v0.3.0 @ 6390 (2.52s, real run — fixture sleeps 2.5s, SKIP path not taken) · full workspace (`--exclude lunaris-py --exclude lunaris-ts`) green except `lunaris-retrieve::tree_recall`, PROVEN pre-existing/environmental: fails identically on a stashed clean tree because the stale Moon v0.2 on default port 6380 accepts connect then lacks `FT._LIST`; passes 3/3 with `MOON_URL=moon://localhost:6390`
- [x] coverage did not decrease — 10 new tests; zero tests removed/weakened
- [x] no test or contract was altered during build — `git log` shows the red-suite commit untouched semantically; `cargo fmt --all` re-wrapped whitespace in `graph_decay_recency.rs` (formatting only, recorded in §5)
- [x] concurrency / timing of the risky operation is safe — `query_raw_with_clauses` borrows the typed connection via `inner_mut()` exactly like the pre-existing `query_raw_valid_at` path; no lock taken, nothing held across `.await`; `GraphDecay` is `Copy`, no shared state
- [x] no exposed secrets, injection openings, or unexpected dependencies — λ/w are constructor-validated finite f64s passed as distinct RESP args (never string-interpolated into Cypher); zero new crates; the existing Cypher-interpolation surface (atomic.rs label/rel guard) is untouched
- [x] layering & dependencies follow CONVENTIONS.md — type lives in lunaris-core, backend override in lunaris-storage-moon, keyspace via `graph_key` helper; additive port method mirrors the `queue_depth` precedent; raw RESP is the documented VALID_AT escape-hatch pattern (typed GraphClient has no flag slots)
- [x] reviewed — auto-resolved under `autonomy: auto` (run.md); no security/concurrency/architecture residue found

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `GraphDecay` exported from `storage::types` → `storage` → crate root; `graph_traverse_decayed` default exercised by core tests, Moon override (`lunaris-storage-moon/src/lib.rs`) dispatches to `graph::graph_traverse_decayed` and is exercised live by all 5 moon-it tests through the `StoragePort` trait object surface. NO retrieval-DSL caller yet — that is the CONTRACTED scope boundary (§1: DSL/SDK surface owned by `ft-navigate-recall`), not dead code: the port method is the deliverable.
- [x] DEAD-CODE (code) — `query_raw_valid_at` retained as a thin delegate (still called by `graph_traverse` as_of path); no orphaned symbol (`cargo clippy --workspace --all-targets` = 0 warnings; `unreachable_pub` deny active in lunaris-storage-moon)
- [x] SEMANTIC (prose) — port.rs/capabilities.rs doc-comments re-read against Moon's decay semantics: cost formula `|weight| + λ·w·age_seconds`, write-Cypher rejection, serde-default rationale all stated and matched by tests

### GATE RECORD
Outcome: PASS (auto-resolved — complete evidence, no escalating residue)
Reviewed by: Claude (ADD auto-gate, autonomy: auto) · date: 2026-06-11

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): rate of `graph_decay_unsupported` / `graph_decay_invalid_*` errors once ft-navigate-recall exposes decay in the DSL; GRAPH.QUERY latency delta with `--decay` on (decay re-costs every relaxed edge — watch p99 on deep traversals).
Spec delta for the next loop (ft-navigate-recall): Lunaris-MERGE-written edges carry Moon's DEFAULT weight (GraphEdge writer sets no WEIGHT arg) — decay over production graphs therefore re-ranks purely by age until the writer threads real weights; ft-navigate-recall should decide whether `FT.NAVIGATE ... DECAY` needs weighted edges or age-only is the intended semantic.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
- [TDD · open] "additive trait method + capability flag" is now a twice-proven recipe (queue_depth, graph_traverse_decayed) — adding a field to the literal-constructed `StorageCapabilities` costs one mechanical perl pass over ~40 sites and the compiler catches every miss; consider `#[non_exhaustive]` + builder only if literal churn becomes a real tax (evidence: clean first compile after the sweep).
- [SDD · open] mirroring the upstream fixture (Moon's DECAY-01/02 in scripts/test-commands.sh) resolved the freeze's lowest-confidence ⚠ before build — WEIGHT is an ADDEDGE argument, not a Cypher property (evidence: §3 flag closed in §4 fixture note; flip test green first run).
- [DDD · open] production GraphEdge writes carry no explicit WEIGHT, so decay on real Lunaris graphs is age-only re-ranking today (evidence: atomic.rs GraphEdge writer; §7 spec delta feeds ft-navigate-recall).
- [ADD · open] stale sibling services on default ports turn graceful-skip tests into hard failures (tree_recall on old Moon @ 6380: connect succeeds, FT._LIST missing) — graceful-skip should distinguish "unreachable" from "reachable but incompatible" (evidence: clean-tree stash repro).
