# TASK: Opt-in SQ8/TQ4 FT quantization with 768d recall eval

slug: sq8-quantization-optin · created: 2026-06-11 · stage: production
phase: done   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Opt-in FT vector quantization — thread Moon's `FT.CREATE … QUANTIZATION <TQ1..TQ4|SQ8>` clause through the `moon://` URL so operators pick the recall/memory point per handle (all per-scope indices inherit), plus a live recall@10 eval at 768-d comparing SQ8 vs the TQ4 server default.

Ground facts (vendor/moon docs/vector-search-guide.md + sdk source, 2026-06-11):
  - QUANTIZATION is an FT.CREATE-time clause; server default is ALREADY TQ4 (~452 B/vec, ~89% recall@10) — Lunaris indices today are TQ4 without saying so. SQ8 = ~900 B/vec, ~98% recall@10. Schema is sticky: an existing index keeps its quantization (same footgun class as dim).
  - The typed SDK's `VectorIndexOptions` has NO quantization slot — opting in requires a raw-RESP FT.CREATE mirroring the SDK's HNSW arg layout with `QUANTIZATION <q>` appended (param_count += 2).
  - Lunaris creates indices in TWO sites: `client.rs::ensure_indexes` (legacy global) and `lib.rs::create_scope_indexes` (per-scope) — both must honor the choice or scopes silently diverge.
  - Quantization applies to compacted HNSW segments; `COMPACT_THRESHOLD` (default 1000) gates when vectors enter quantized storage — the eval must force compaction (low threshold + FT.COMPACT) or it measures the un-quantized write buffer.

Framings weighed: URL query param `?quant=sq8` on the existing `moon://` grammar (chosen — `?ws=` precedent; flows through every constructor incl. SDK consumers via storage URL, no new API surface; per-handle value, per-scope indices inherit) · new `connect_with_opts` constructor (API churn across MoonStorage/open.rs/SDKs for one knob) · env var (invisible in the URL-centric `lunaris::open` story, per-process not per-handle) · expose in lunaris-core capabilities/config (wrong layer — Moon-specific creation knob).
Scope boundary: Moon backend only; no lunaris-core type changes, no capability flag (creation-time knob, not a runtime feature). Re-quantizing an EXISTING index is out of scope (sticky-schema; documented like the dim footgun).
Must:
<must>
  - `Quantization` enum in lunaris-storage-moon (`Tq1..Tq4, Sq8`) with case-insensitive `FromStr` (`"sq8"`, `"tq4"`, …) and wire rendering (`"SQ8"`, …)
  - URL grammar extends to `moon://host:port[?ws=…][&quant=<q>]`; parsed BEFORE network IO; recorded on `MoonClient.quantization: Option<Quantization>`
  - When `Some(q)`, BOTH index-creation sites issue FT.CREATE via raw RESP with `QUANTIZATION <q>` inside the HNSW param block (param_count adjusted), preserving every existing schema field (content TEXT, valid_time NUMERIC + source TAG on chunks); when `None`, behavior is byte-identical to today (SDK path, server default TQ4)
  - "already exists" stays idempotent-swallowed on the raw path (sticky schema documented)
  - Live recall@10 eval at 768-d (moon-it): deterministic corpus, compaction forced, SQ8 index recall@10 ≥ 0.90 floor AND ≥ TQ4-default recall (regression guard for the SQ8-is-higher-recall claim), verdict numbers printed
</must>
Reject:
<reject>
  - unknown quant value (`?quant=foo`) -> StorageError::Backend("moon_invalid_quantization: …") BEFORE any network IO
  - `quant` on a non-moon scheme -> existing UnsupportedScheme behavior unchanged (scheme check runs first)
  - re-opening a handle with a DIFFERENT quant against existing indices -> NOT detected (FT.INFO has no quantization probe on v0.3.0) — documented sticky-schema footgun, mirrors the dim guardrail's (a)/(b) recipe in prose
</reject>
After:
<after>
  - `lunaris::open("moon://…?quant=sq8")` (and the py/ts SDKs via their storage URLs, zero SDK changes) provisions all per-scope indices at SQ8
  - bench-rerun-v030 can A/B TQ4 vs SQ8 recall + RSS using only the URL
  - default deployments are untouched (None -> server default TQ4)
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Moon v0.3.0 actually accepts `QUANTIZATION` in the FT.CREATE HNSW param block at 768-d and FT.COMPACT quantizes accordingly — lowest confidence because the working-SQ8 claim comes from release notes and the 384-d validation; if wrong: the eval test stays red and the FIXTURE/claim adjusts (maybe SQ8 unusable at 768-d → milestone learns that, contract shape unaffected)
  ⚠ recall@10 floor 0.90 for SQ8 at 768-d on a random-Gaussian corpus is achievable (docs claim ~98% but on real-ish distributions) — if wrong: floor is evidence-tuned in the tests phase BEFORE freeze-breaking; the SQ8 ≥ TQ4 comparative assertion is the real contract
  - [ ] raw FT.CREATE arg layout matches the SDK's exactly (mirrored from sdk/rust/src/vector.rs:29-58) — compile-time copy, verified live by the eval test
  - [ ] `url::Url::query_pairs` yields `quant` irrespective of `ws` ordering
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: quant URL param provisions quantized per-scope indices (live Moon)
  Given a handle opened with moon://…?quant=sq8
  When a first write triggers create_scope_indexes for a fresh scope
  Then FT.CREATE carries QUANTIZATION SQ8 (observable: vector round-trip works and the eval
       measures SQ8-grade recall on that index)
  And a handle WITHOUT the param behaves byte-identically to today (server-default TQ4)

Scenario: invalid quant value rejected before IO
  Given moon://localhost:1?quant=foo (port intentionally dead)
  When MoonStorage::connect runs
  Then it errors "moon_invalid_quantization" WITHOUT attempting a connection
  And ?quant=SQ8 / ?quant=sq8 / ?quant=tq2 all parse (case-insensitive)

Scenario: recall@10 eval — SQ8 ≥ TQ4 default at 768-d (live Moon)
  Given two fresh indices over the same deterministic 768-d corpus, compaction forced,
        one created with QUANTIZATION SQ8 and one with the server default (TQ4)
  When recall@10 is measured against brute-force ground truth for N queries
  Then SQ8 recall@10 >= 0.90 AND SQ8 recall@10 >= TQ4 recall@10
  And both numbers are printed as the eval verdict

Scenario: ws and quant params coexist
  Given moon://host:6390?ws=hot&quant=sq8 (and the reverse order)
  When the URL parses
  Then workspace == "hot" AND quantization == Sq8
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
NEW TYPE (crates/lunaris-storage-moon/src/client.rs):
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub enum Quantization { Tq1, Tq2, Tq3, Tq4, Sq8 }
    impl FromStr   — case-insensitive "tq1".."tq4" | "sq8"; else Backend("moon_invalid_quantization: got '<v>', expected tq1|tq2|tq3|tq4|sq8")
    fn as_wire(&self) -> &'static str   // "TQ1".."TQ4" | "SQ8"

URL GRAMMAR: moon://host:port[?ws=<workspace>][&quant=<q>]    (param order irrelevant)
  MoonClient gains `pub quantization: Option<Quantization>`; parse error surfaces BEFORE network IO
  (after scheme + dim checks, before TypedClient::connect)

INDEX CREATION (both client.rs::ensure_indexes AND lib.rs::create_scope_indexes):
  quantization == None  -> SDK typed create_index (unchanged, server default TQ4)
  quantization == Some(q) -> raw RESP FT.CREATE mirroring the SDK arg layout
    (ON HASH PREFIX 1 <prefix> SCHEMA <extra fields> vec VECTOR HNSW <param_count>
     TYPE FLOAT32 DIM <dim> DISTANCE_METRIC COSINE M 16 EF_CONSTRUCTION 200
     QUANTIZATION <q>)            // param_count = 12
  "already exists" swallowed on both paths (sticky schema — documented)

EVAL (crates/lunaris-storage-moon/tests/quantization_recall.rs, moon-it):
  deterministic seeded corpus (768-d, N≈600, COMPACT_THRESHOLD 100 + FT.COMPACT),
  recall@10 vs brute-force cosine ground truth over ≥20 queries;
  assert sq8 >= 0.90 && sq8 >= tq4_default; print "QUANT EVAL VERDICT: sq8=… tq4=…"

Error responses:
  moon_invalid_quantization -> StorageError::Backend, at URL parse, no IO
  (no other new codes; existing already-exists/idempotency semantics unchanged)

Schema: no lunaris-core change, no capability flag, no persistent data migration.
Docs: sticky-quantization footgun paragraph appended to docs/migration/0.2-to-0.3-optional-embedder.md
      §index-recreate recipe OR a short docs/migration/0.7-quantization.md (build-time choice).
```

Status: FROZEN @ v1 — approved by Tin Dang (2026-06-11, freeze #4)
Least-sure flag surfaced at freeze:
  ⚠ [spec] Moon v0.3.0's FT.CREATE QUANTIZATION acceptance at 768-d is release-note-claimed, unprobed — if wrong: eval stays red, milestone learns SQ8's real envelope; contract shape unaffected.
  ⚠ [test] the 0.90 recall@10 floor on a synthetic Gaussian corpus may need evidence-tuning pre-build; the SQ8 ≥ TQ4 comparative assertion is the binding guard.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every §2 scenario has an executable test; red = compile failure on the missing Quantization type / quantization field + the unit URL-parse assertions.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - unit (client.rs tests or tests/quantization_url.rs): from_str accepts tq1..tq4/sq8 case-insensitively, rejects "foo"/"" with moon_invalid_quantization; connect("moon://localhost:1?quant=foo") errors BEFORE IO (dead port proves no connect attempt); ws+quant order-independent parse
  - moon-it quantization_recall.rs::sq8_beats_or_matches_tq4_recall_at_768d: two fresh raw FT.CREATE indices (one QUANTIZATION SQ8 via the new path, one default), seeded deterministic corpus, FT.COMPACT, recall@10 floor + comparative assert, verdict printed  [DISCRIMINATOR + resolves ⚠ #1]
  - moon-it quantization_recall.rs::quant_handle_provisions_scoped_indexes: MoonStorage::connect("…?quant=sq8") + one atomic_write VectorUpsert to a fresh scope + vector_search round-trips (proves the per-scope creation path accepts the clause)
</test_plan>

Tests live in: `crates/lunaris-storage-moon/tests/quantization_url.rs` · `crates/lunaris-storage-moon/tests/quantization_recall.rs` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): both index-creation sites MUST share one schema body — a `?quant=` handle whose global and per-scope indices diverge on quantization or schema fields is a silent correctness bug.
Code lives in: `crates/lunaris-storage-moon/src/{client.rs, lib.rs}` · `docs/migration/0.7-quantization.md`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

Build notes:
- Implemented as one shared helper `client::create_lunaris_index[_named]` (None → typed-SDK path preserving the exact pre-change byte layout incl. the structural-test literals; Some(q) → raw RESP FT.CREATE mirroring vendor/moon/sdk/rust/src/vector.rs::create_index with QUANTIZATION appended, param_count 10→12). `ensure_indexes` and `create_scope_indexes` both delegate — the two creation sites can no longer diverge (stronger than the contract's "both must honor", same observable behavior).
- `?quant=` parse is `query_pairs().find(…).map(parse).transpose()?` placed after scheme+dim checks, before the redis dial — `tests/quantization_url.rs::invalid_quant_rejected_before_io` proves it on a dead port.
- Only fmt-rewrap touched a test file post-red (eprintln line width); zero behavioral test edits, contract untouched.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — quantization_url 4/4; quantization_recall 3/3 live vs Moon v0.3.0 @6390 (1.23–1.57s wall, verdict line printed → no silent skip); lib unit 67/67; all other moon-it suites green vs 6390 (episode 3, graph_decay 5, keyword 3, smoke 7, mq 6+2, navigate 6, scope 1); workspace test sweep green except two pre-existing environmental failures (below)
- [x] coverage did not decrease — 7 new tests added, none removed
- [x] no test or contract was altered during build — only `cargo fmt` line-rewrap of quantization_recall.rs eprintln (no assertion/logic change); §3 untouched
- [x] concurrency / timing safe — no new locks; helper takes `&TypedClient` and clones per call (multiplexed conn, same pattern as every other call site); no lock held across .await
- [x] no exposed secrets / injection / unexpected deps — `quant` value never interpolated raw: parsed into a closed enum before any wire use; `as_wire()` returns only the five static uppercase strings; no new dependencies
- [x] layering follows conventions — Quantization lives in lunaris-storage-moon (Moon-specific creation knob, NOT lunaris-core, per §1 scope boundary); no capability flag added
- [x] reviewed — auto-resolved under autonomy:auto; manual diff review of client.rs/lib.rs done before commit

Pre-existing environmental failures (NOT this task, both root-caused previously):
- `dim_configurable` 2/3: within-suite race — `reopen_at_mismatched_dim_is_rejected` creates 768-d globals while `vector_upsert_and_search_at_1536_dim` races its connect; PROVEN flaky on BOTH pre-change (1 fail / 3 fresh-server runs) and post-change (2 fail / 3 runs) code via git-stash A/B on fresh ports 6392-6394. Extends the recorded cross-suite-pollution delta.
- `tree_recall` 2 fails in default-port workspace sweep (stale Moon v0.2 on 6380); 3/3 green vs MOON_URL=6390 re-run this session.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `Quantization` parsed in `connect_with_dim` → stored on `MoonClient.quantization` → read by BOTH `ensure_indexes` (client.rs) and `create_scope_indexes` (lib.rs) via `create_lunaris_index[_named]`; production path proven by `quant_handle_provisions_scoped_indexes` (connect with ?quant=sq8 → atomic_write → vector_search round-trip) and by tq4=0.405 vs sq8=0.995 divergence (the two handles demonstrably created differently-quantized indices through the production ensure_scope path)
- [x] DEAD-CODE (code) — no orphans: clippy --workspace --all-targets clean; `create_lunaris_index` (kind==name arm) used by ensure_indexes, `_named` by create_scope_indexes; all enum variants reachable via FromStr
- [x] SEMANTIC (prose) — docs/migration/0.7-quantization.md read in full after writing: tier table matches vendor/moon docs; sticky-footgun recipe mirrors the dim footgun's (a)/(b) shape per §1 Reject; synthetic-vs-real-recall caveat included so the 0.405 number isn't misread as a TQ4 production claim

### GATE RECORD
Outcome: PASS
Reviewed by: auto-resolved (autonomy: auto; complete evidence, no security finding, no concurrency/architecture residue) · date: 2026-06-11

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): QUANT EVAL VERDICT line in CI moon-it runs (sq8 floor 0.90 + sq8≥tq4 comparative); operator reports of recall drop after FT.COMPACT on `?quant=` handles (write-buffer-vs-compacted settling documented in 0.7-quantization.md)
Spec delta for the next loop: TQ4 default measured 0.405 recall@10 on a random-uniform 768-d corpus (vs its documented ~0.89 on real embeddings) — when the bench-rerun-v030 task lands, re-measure with real SQuAD embeddings before deciding whether Lunaris should flip its DEFAULT to sq8; also Moon could expose quantization in FT.INFO to enable a connect-time guardrail like dim.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
- [TDD · folded] dim_configurable has a WITHIN-suite race, not just cross-suite pollution: reopen-768-baseline vs 1536-connect on a fresh server is order/race-dependent — flaky on both pre- and post-change code (3× fresh-server A/B, 2026-06-11); fix = serialize the suite or give the 1536 test its own server/port
- [TDD · folded] synthetic random vectors invert quantization-tier rankings (tq4 0.405 vs documented 0.89) — recall evals comparing tiers MUST note corpus realism or they mislead default-choice decisions (evidence: QUANT EVAL VERDICT 2026-06-11)
- [SDD · folded] Moon FT.INFO has no quantization field, so sticky-quantization cannot get the dim-style connect-time guardrail — upstream Moon feature request candidate (evidence: §1 Reject row 3, probe 2026-06-11)
- [ADD · folded] probe-before-freeze (live /tmp/quant_probe.py confirming QUANTIZATION accepted at 768-d, param_count 12) converted both freeze ⚠ flags into ground facts before red tests were written — cheap de-risking worth keeping (evidence: task froze and built green in one pass)
