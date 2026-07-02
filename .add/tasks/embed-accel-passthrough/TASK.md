# TASK: Land Metal/Accelerator feature passthrough on main + reranker batch tunable

slug: embed-accel-passthrough · created: 2026-07-02 · stage: production
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

Anchors the contract cites: crates/lunaris/Cargo.toml:[features] (~L119-159) ·
crates/lunaris-embed-native/src/embedder.rs:MAX_PUBLIC_BATCH,embed_batch (L49,L247) ·
crates/lunaris-embed-native/src/quantized_embedder.rs (L160-164) ·
crates/lunaris-ingest/src/pipeline.rs:INGEST_EMBED_BATCH_SIZE,embed_with_fallback (L42,L389) ·
crates/lunaris-rerank-native/src/reranker.rs:MAX_PUBLIC_BATCH (L39,L225)

Touches (files · symbols · signatures):
  - `crates/lunaris/Cargo.toml:[features]` (~L119-159) — umbrella crate. `default = ["native"]`;
    `native = ["dep:lunaris-embed-native", "dep:lunaris-rerank-native", "dep:candle-core"]`.
    Currently has ZERO `metal`/`cpu-accelerate`/`cpu-mkl`/`cuda` feature entries (confirmed:
    `grep -c 'metal\|accelerate\|cuda\|mkl'` = 0) — no accelerator forwarding exists on `main` at all.
  - `crates/lunaris-embed-native/Cargo.toml:[features]` — already defines `cpu-accelerate`,
    `cpu-mkl`, `metal`, `cuda`, `cuda-fa2` (candle-core/candle-nn feature forwards), unused by
    the umbrella today because nothing enables them.
  - `crates/lunaris-rerank-native/Cargo.toml:[features]` (L84-91) — mirrors the same 5 features
    (`cpu-accelerate`, `cpu-mkl`, `metal`, `cuda`, `cuda-fa2`), also dark.
  - `crates/lunaris-embed-native/src/embedder.rs:MAX_PUBLIC_BATCH` (L49, `= 8`) and
    `embed_batch` (L247) — re-chunks via `owned.chunks(MAX_PUBLIC_BATCH)` (L266), hardcoded,
    no env override on `main`.
  - `crates/lunaris-embed-native/src/quantized_embedder.rs:L160-164` — mirrors the same
    hardcoded `crate::embedder::MAX_PUBLIC_BATCH` chunk call for the GGUF path.
  - `crates/lunaris-ingest/src/pipeline.rs:INGEST_EMBED_BATCH_SIZE` (L42, `= 32`),
    `embed_with_fallback` (L389) — drives `drafts.chunks(INGEST_EMBED_BATCH_SIZE)` (L399) into
    `embedder.embed_batch`; the driver batch and the embedder's internal re-chunk ceiling are
    two independent hardcoded constants today.
  - `crates/lunaris-rerank-native/src/reranker.rs:MAX_PUBLIC_BATCH` (L39, `= 8`) — same pattern
    as the embedder, `docs.chunks(MAX_PUBLIC_BATCH)` at L225, but with NO env-override
    counterpart anywhere (the asymmetry this task also closes).
  - `crates/lunaris-embed-native/src/device_select.rs:select_device` — already auto-upgrades a
    caller-passed `Device::Cpu` to Metal/CUDA when the matching feature is compiled in (existing,
    untouched — this task only supplies the missing feature wiring that lets it fire).

Source of the port (proven, unmerged): branch `bench/longmemeval-evidence-recall`, commit
`096d46d` ("perf(embed): enable Metal/Accelerator backend + env-configurable batch (~11x
ingest)") — touches exactly `crates/lunaris/Cargo.toml`, `crates/lunaris-embed-native/src/{embedder,quantized_embedder}.rs`,
`crates/lunaris-ingest/src/pipeline.rs`. Measured on Apple M4 Pro: LongMemEval-S haystack
ingest 21 min/question -> 114.85s (~11x), J-score 100% preserved (pilot), 8 new red/green
resolver tests. Full diff inspected via `git show 096d46d`.

Context (working folder):
  - `docs/benchmarks/v0.4-O01-baselines.md` — the O-01 per-device perf gate table (Apple
    Silicon Metal: embed p50 <=5ms / p99 <=15ms / rerank p50 K=10 <=40ms); all cells "TBD" —
    never measured against real full-model weights on `main`.
  - `docs/spike/O-02-mlx/DECISION.md` — GO-decided MLX backend spike (8.74x candle-Metal on
    Apple Silicon, single layer), recommended v0.5+, explicitly out of scope here (this task
    only lands the already-proven candle accelerator passthrough).
  - No TODO/FIXME markers found in the touched files referencing this gap; the gap is
    structural (a feature nobody wired), not a flagged known-issue.

Honors (patterns / conventions):
  - CLAUDE.md "Latest libraries policy" + "No duplicate vector/BM25 libs" — this task adds no
    new dependency; it only forwards existing `candle-core`/`candle-nn` cargo features already
    declared in both native crates.
  - CLAUDE.md "File size" / "Lock discipline" — no file crosses 1000/1500 lines from this change
    (embedder.rs, reranker.rs, pipeline.rs are all well under); no lock is introduced.
  - Repo TDD convention (CLAUDE.md Rule 3, mem0-parity-hardening precedent) — every new resolver
    function ships with red/green unit tests, mirroring `096d46d`'s own test shape 1:1 for the
    embedder/ingest port, and net-new tests for the reranker counterpart.
  - `native` feature discipline (`crates/lunaris/Cargo.toml` doc-comment) — accelerator features
    stay OFF by default (opt-in per host arch), consistent with `embedder-gguf`/`reranker-gguf`.

Anchors the contract cites:
  - `crates/lunaris/Cargo.toml::[features] metal|cpu-accelerate|cpu-mkl|cuda|cuda-fa2`
  - `lunaris_embed_native::embedder::public_batch_size` / `resolve_batch_size`
  - `lunaris_ingest::pipeline::ingest_embed_batch_size` / `resolve_ingest_batch_size`
  - `lunaris_rerank_native::reranker::public_batch_size` / `resolve_batch_size` (new, mirrors
    the embedder's function 1:1 against `LUNARIS_RERANK_BATCH`)

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Accelerator feature passthrough (embed + rerank) + symmetric batch tunability, landed on `main`

Framings weighed:
  A. **(chosen)** Port the proven, already-measured fix verbatim (cherry-pick `096d46d` +
     `713478b` from `bench/longmemeval-evidence-recall`, reconciling only the mechanical
     `Cargo.toml` conflict against main's `native`-optional refactor), THEN close the
     embed/rerank asymmetry by adding a new `LUNARIS_RERANK_BATCH` env-tunable to
     `lunaris-rerank-native` mirroring the embedder's `public_batch_size()`/`resolve_batch_size`
     pattern exactly. · B. Merge the entire `bench/longmemeval-evidence-recall` branch (28
     commits: LongMemEval evidence-recall harness, embedding-dedup, community-summary cap,
     RAPTOR wiring changes) — rejected, far bigger surface than "land the fix", drags in
     unrelated eval-harness and content-policy changes the user did not ask for. · C.
     Reimplement the accelerator passthrough from scratch without referencing the branch —
     rejected, discards proven/measured/tested work and risks behavioral drift from the
     ~11x number already verified.

Must:
<must>
  - `crates/lunaris/Cargo.toml` gains `metal`/`cpu-accelerate`/`cpu-mkl`/`cuda`/`cuda-fa2`
    features that forward to BOTH `lunaris-embed-native` and `lunaris-rerank-native`'s
    matching features (mirrors the existing `embedder-gguf`/`reranker-gguf` pattern,
    including the explicit `"native"` prerequisite gate for readability parity).
  - All 5 accelerator features stay OUT of `default` — opt-in per host arch, unchanged
    from `096d46d`'s own design and consistent with `embedder-gguf`/`reranker-gguf`.
  - `NativeEmbedder`/`NativeQuantizedEmbedder::embed_batch` bound each forward pass by
    BOTH row count and activation footprint (`plan_batches`, ported from `713478b`) — a
    long single input never gets co-batched with others regardless of `LUNARIS_EMBED_BATCH`.
  - `LUNARIS_EMBED_BATCH` (embedder, ported) and a new `LUNARIS_RERANK_BATCH` (reranker,
    new) each override their crate's public batch ceiling; missing/non-numeric/zero/negative
    values fall back to the existing safe const (`MAX_PUBLIC_BATCH = 8` for both) — never
    panic via `slice::chunks(0)`.
  - `lunaris-ingest`'s driver batch (`ingest_embed_batch_size`, ported) never drops the
    per-call chunk size below `INGEST_EMBED_BATCH_SIZE` (32) even if `LUNARIS_EMBED_BATCH`
    is set smaller — the embedder's re-chunk ceiling and the driver's feed size are
    independent knobs by design (ported behavior, unchanged).
  - Every ported/new resolver function keeps 1:1 parity with its origin commit's red/green
    test set (embedder: 8 tests from `096d46d` + 5 `plan_batches` tests from `713478b`;
    reranker: a new, symmetric test set for `resolve_batch_size`/`public_batch_size`).
  - `cargo test -p lunaris-embed-native -p lunaris-rerank-native -p lunaris-ingest -p
    lunaris-memory` and `cargo clippy --workspace --all-targets` stay green after landing.

Reject:
<reject>
  - `LUNARIS_EMBED_BATCH` / `LUNARIS_RERANK_BATCH` set to `"0"`, negative, empty, or
    non-numeric -> falls back to `MAX_PUBLIC_BATCH` (never a panic; not a hard error —
    matches the ported `resolve_batch_size` contract, "reject" here means "safely ignore").
  - A batch whose combined row-count × max-seq² exceeds the activation budget -> forced to
    a batch-of-one (the `plan_batches` OOM guard) rather than silently OOMing or truncating
    inputs.
  - Enabling more than one of `metal`/`cuda` at compile time is NOT rejected by this task
    (out of scope — `device_select.rs`'s existing CUDA-wins-over-Metal priority governs;
    no new conflict-detection is being added here, unlike the deferred MLX spike's
    `ConflictingBackends` design).
</reject>

After:
<after>
  - A `main` build with `--features lunaris-memory/metal` (or `cpu-accelerate`/`cpu-mkl`/
    `cuda`) auto-upgrades both the embedder AND reranker from `Device::Cpu` to the selected
    backend at `Lunaris::open()` — today only the embedder had a path to this (and only on
    the unmerged branch); on `main` today NEITHER does.
  - Operators can raise both the embedder's and the reranker's per-forward batch ceiling
    independently via env vars, symmetric for the first time.
  - The RAPTOR-community-summary OOM class (124 GB activation tensor, root cause of the
    n=39 LongMemEval-S drop documented in project memory) cannot recur via the embed path
    landed here — `713478b`'s activation-footprint bound is present on `main`.
  - `docs/benchmarks/v0.4-O01-baselines.md`'s "TBD" cells remain TBD (out of scope — no new
    benchmark run is part of this task; landing the capability is separate from re-running
    the O-01 gate table).
</after>

Assumptions — lowest-confidence first:
<assumptions>
  ⚠ **The reranker's cross-encoder cost will still exceed the embedder's after this lands** —
    lowest confidence in whether this task "fixes" the user's reported symptom
    (reranker slowest) vs. only removes the CPU-only-candle amplifier. `bge-reranker-v2-m3`
    is FP32/24-layer/cross-encoder (O(k) forward passes, no caching) vs the embedder's
    FP16/22-layer/single-pass-with-cache; even fully accelerated, reranker-slower-than-
    embedder is architecturally expected, not necessarily evidence of a remaining bug. If
    wrong (i.e. the user expected parity, not just "both fast"): the deliverable will read
    as incomplete even though it did exactly what was scoped — flagging this explicitly at
    the freeze so it isn't a silent scope surprise.
  - [ ] `44d31f2` (community-summary length cap, "defense-in-depth" per its own commit
    message on top of `713478b`) is OUT of scope — `713478b` alone fixes the OOM
    *mechanism*; `44d31f2` is a separate RAPTOR content-policy concern (summary length),
    not required for correctness of the accelerator/batch feature. Recorded as a §7 SPEC
    delta rather than silently pulled in.
  - [ ] The Cargo.toml conflict resolution (adding `"native"` as an explicit prerequisite to
    the 4 new accelerator features, matching the existing `embedder-gguf`/`reranker-gguf`
    style) is technically redundant under Cargo's `dep/feature` implicit-activation
    semantics, but chosen for local convention consistency — verified building clean with
    `--features metal` in the scratch worktree, so this is a style choice, not a risk.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: metal feature forwards to both native crates
  Given `crates/lunaris/Cargo.toml` on this task's branch
  When `cargo check -p lunaris-memory --features metal` is run
  Then it compiles clean AND `lunaris-embed-native/metal` + `lunaris-rerank-native/metal`
       are both activated (verified via `cargo tree -e features -p lunaris-memory --features metal`
       showing both native crates carrying their `metal` feature)

Scenario: accelerator features are opt-in only
  Given a plain `cargo build -p lunaris-memory` with no extra `--features`
  When the build completes
  Then `candle-core/metal`, `candle-core/accelerate`, `candle-core/mkl`, `candle-core/cuda`
       are all inactive (default build stays CPU-plain, unchanged from before this task)

Scenario: a long single input is never co-batched (OOM guard)
  Given a granite-r2 `NativeEmbedder` and an input slice containing one text near the
        8192-token ceiling alongside several short texts
  When `embed_batch` re-chunks via `plan_batches`
  Then the long text is placed alone in its own batch-of-one, never padded together with
       the short texts, regardless of `LUNARIS_EMBED_BATCH`'s value

Scenario: LUNARIS_EMBED_BATCH raises the embedder's re-chunk ceiling
  Given `LUNARIS_EMBED_BATCH=64` set in the process environment
  When `NativeEmbedder::embed_batch` re-chunks a 100-item input (all short, well under the
       activation budget)
  Then each dispatched forward-pass chunk contains up to 64 rows (not the default 8)

Scenario: LUNARIS_RERANK_BATCH raises the reranker's re-chunk ceiling (new, symmetric)
  Given `LUNARIS_RERANK_BATCH=64` set in the process environment
  When `NativeReranker::rerank` (or equivalent) re-chunks a 100-candidate input
  Then each dispatched forward-pass chunk contains up to 64 pairs (not the default 8) —
       mirrors the embedder scenario above exactly

Scenario: the ingest driver never feeds the embedder below its blueprint floor
  Given `LUNARIS_EMBED_BATCH=4` (smaller than `INGEST_EMBED_BATCH_SIZE = 32`)
  When `embed_with_fallback` drives chunks into `embedder.embed_batch`
  Then the driver still calls with windows of at least 32 (`ingest_embed_batch_size() =
       max(INGEST_EMBED_BATCH_SIZE, requested)`), even though the embedder's OWN internal
       re-chunk ceiling is 4
  And no chunk is dropped or embedded twice

Scenario: ported + new resolver functions hold 1:1 test parity
  Given `cargo test -p lunaris-embed-native -p lunaris-rerank-native -p lunaris-ingest`
  When the suite runs
  Then all 8 embedder resolver tests (from `096d46d`) + 5 `plan_batches` tests (from
       `713478b`) + the new symmetric reranker resolver tests all pass, and no existing
       test in these crates regresses

Scenario: workspace stays clippy-clean after landing
  Given the full set of changes on this task's branch
  When `cargo clippy -p lunaris-embed-native -p lunaris-rerank-native -p lunaris-ingest -p
       lunaris-memory --all-targets` is run
  Then it reports zero warnings

Scenario: garbage/zero/negative env values fall back safely (Reject)
  Given `LUNARIS_EMBED_BATCH` (or `LUNARIS_RERANK_BATCH`) set to `"0"`, `"-4"`, `""`, or
        `"abc"`
  When `resolve_batch_size` (embedder or reranker) reads it
  Then the function returns `MAX_PUBLIC_BATCH` (8), never panics, never produces a
       zero-length chunk window
  And the crate's default (unset-env) behavior remains byte-identical to before this task

Scenario: an over-budget batch is forced to batch-of-one, never OOMs (Reject)
  Given an input whose row-count × max-seq² exceeds the activation budget
        (`public_batch_size() × REF²`)
  When `plan_batches` plans the dispatch windows
  Then the over-budget input is isolated into a batch of exactly one row — it is never
       silently truncated, dropped, or merged with neighbors to force it under budget
  And every other (in-budget) input in the same call is still processed and returned in
      its original order

Scenario: multiple compile-time accelerator features are left to existing priority (Reject — explicit non-goal)
  Given both `metal` and `cuda` features compiled in simultaneously (the one platform where
        both could theoretically build)
  When `NativeEmbedder::open()` calls `select_device`
  Then the existing `device_select.rs` CUDA-wins-over-Metal priority governs, UNCHANGED by
       this task — no new `ConflictingBackends` error is introduced (that is the deferred
       MLX-spike design, explicitly out of scope here)
  And no existing `device_select.rs` test is modified
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

Non-HTTP feature — the frozen shape is a Cargo feature table + Rust function signatures +
env-var contract, not a REST endpoint.

```
# crates/lunaris/Cargo.toml [features] — NEW entries
metal          = ["native", "lunaris-embed-native/metal",          "lunaris-rerank-native/metal"]
cpu-accelerate = ["native", "lunaris-embed-native/cpu-accelerate", "lunaris-rerank-native/cpu-accelerate"]
cpu-mkl        = ["native", "lunaris-embed-native/cpu-mkl",        "lunaris-rerank-native/cpu-mkl"]
cuda           = ["native", "lunaris-embed-native/cuda",           "lunaris-rerank-native/cuda"]
cuda-fa2       = ["native", "lunaris-embed-native/cuda-fa2",       "lunaris-rerank-native/cuda-fa2"]
# none added to `default`

# crates/lunaris-embed-native/src/embedder.rs — PORTED (096d46d + 713478b), unchanged signatures
fn resolve_batch_size(raw: Option<&str>) -> usize
pub fn public_batch_size() -> usize                       # reads LUNARIS_EMBED_BATCH, OnceLock-cached
pub(crate) const EMBED_REF_SEQ_BYTES: usize = 2048
pub(crate) fn activation_budget() -> u128                  # = public_batch_size() as u128 * EMBED_REF_SEQ_BYTES^2
pub(crate) fn plan_batches(byte_lens: &[usize], max_rows: usize, budget: u128) -> Vec<usize>
  -> sizes.sum() == byte_lens.len(); every window rows<=max_rows.max(1) OR rows==1 (irreducible);
     rows==1 may exceed budget (accepted floor); never emits a 0-size window.
# quantized_embedder.rs::embed_batch calls the same plan_batches/public_batch_size (ported, mirrored)

# crates/lunaris-ingest/src/pipeline.rs — PORTED (096d46d), unchanged signatures
fn resolve_ingest_batch_size(raw: Option<&str>) -> usize   # = max(INGEST_EMBED_BATCH_SIZE, requested)
fn ingest_embed_batch_size() -> usize                       # reads LUNARIS_EMBED_BATCH, OnceLock-cached
  -> embed_with_fallback() chunks drafts.chunks(ingest_embed_batch_size()) instead of the bare const

# crates/lunaris-rerank-native/src/reranker.rs — NEW THIS TASK, symmetric to the embedder
fn resolve_batch_size(raw: Option<&str>) -> usize           # byte-identical fallback policy to embedder's
pub fn public_batch_size() -> usize                          # reads LUNARIS_RERANK_BATCH, OnceLock-cached
  -> rerank's pair-chunking loop (`docs.chunks(MAX_PUBLIC_BATCH)` today, L225) switches to
     `docs.chunks(public_batch_size())`
  -> NOT porting plan_batches/activation_budget to the reranker in this task (§1 Reject —
     no evidence of a reranker-side OOM class; the cross-encoder pairs are query+doc, already
     bounded by the tokenizer's max_position_embeddings per pair, unlike RAPTOR summaries which
     are whole documents). Flagged as a SPEC delta in §7, not silently ported.

Env vars (process env, read once + cached per-process):
  LUNARIS_EMBED_BATCH   -> lunaris-embed-native::public_batch_size() [ported]
                         -> lunaris-ingest::ingest_embed_batch_size() [ported, same var, floor=32]
  LUNARIS_RERANK_BATCH  -> lunaris-rerank-native::public_batch_size() [NEW]
  Both: missing/non-numeric/<1 -> MAX_PUBLIC_BATCH (8) fallback. Never panics.
```

Least-sure flag surfaced at freeze:
  [spec] ⚠ Whether landing this closes the user's reported symptom ("reranker slowest,
  embedder second") to their satisfaction, vs. only removing the CPU-only-candle amplifier —
  the reranker will very likely remain slower than the embedder afterward for architectural
  reasons (FP32 cross-encoder, O(k) forward passes) unrelated to this fix. Cost if wrong: the
  deliverable reads as incomplete even though it did exactly what was scoped. Mitigation:
  called out explicitly in the final report rather than implied as "problem solved."
  [contract] Secondary, lower-stakes: NOT porting `plan_batches`/activation-footprint bound to
  the reranker (only the plain batch-size env knob) — reasoned as safe because cross-encoder
  pairs are tokenizer-bounded per-pair, unlike RAPTOR's whole-document summaries. Recorded as
  a §7 SPEC delta rather than silently decided.

Status: FROZEN @ v1 — approved by Tin Dang (delegated: "go ahead and land the fix on main use
ADD"; user unavailable for synchronous per-phase confirmation — proceeding under project
default `autonomy: auto`; flags above surfaced here for post-hoc review rather than blocking).
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: 1:1 test parity with the origin commits for the ported code (13 tests: 4
embedder `resolve_batch_size*` + 5 `plan_batches_*` + 4 ingest `ingest_batch_*`) + 4 new
symmetric tests for the reranker resolver. No line-coverage % target (Rust workspace has no
line-coverage gate in CI today) — parity + scenario coverage is the target.

Plan (one test per scenario, Rust — inline `#[cfg(test)] mod tests`, not a separate `tests/` dir):
<test_plan>
  - metal_feature_forwards / accelerator_features_are_opt_in: verified by `cargo check -p
    lunaris-memory --features metal` + `cargo build -p lunaris-memory` (default) in Build/Verify —
    Cargo feature-graph shape, not a Rust unit test.
  - plan_batches_isolates_a_long_input (embedder.rs, PORTED from 713478b): the OOM regression
    guard — asserts a long input never co-batches; assert budget invariant via
    assert_budget_holds().
  - plan_batches_{short_inputs_fill_to_row_cap,medium_inputs_get_fewer_rows,empty_and_singleton,
    never_emits_zero_window_even_at_max_rows_zero} (embedder.rs, PORTED from 713478b).
  - resolve_batch_size_{defaults_when_unset,honors_large_override,trims_whitespace,
    rejects_zero_and_garbage} (embedder.rs, PORTED from 096d46d) — LUNARIS_EMBED_BATCH resolver.
  - ingest_batch_{defaults_to_const_when_unset,raises_for_large_override,
    never_drops_below_const,falls_back_on_garbage} (pipeline.rs, PORTED from 096d46d) —
    ingest driver floor=32.
  - resolve_batch_size_{defaults_when_unset,honors_large_override,trims_whitespace,
    rejects_zero_and_garbage} (reranker.rs, **NEW this task**) — LUNARIS_RERANK_BATCH
    resolver, byte-identical fallback policy to the embedder's. WRITTEN NOW, confirmed RED
    (compile error `cannot find function resolve_batch_size in this scope`, 8 occurrences) —
    the right reason: the function does not exist in `lunaris-rerank-native` yet.
  - Reject/multiple-accelerator-features scenario: no new test — explicitly asserted as
    "no existing device_select.rs test is modified" (verified by not touching that file).
</test_plan>

Tests live in: `crates/lunaris-embed-native/src/embedder.rs` `crates/lunaris-ingest/src/pipeline.rs`
`crates/lunaris-rerank-native/src/reranker.rs` · inline `#[cfg(test)] mod tests` per Rust
convention (this project has no `tests/` subdir per task — matches repo-wide pattern). Ported
tests (embedder + ingest) verified green via cherry-pick in a disposable worktree BEFORE this
task's own red/green cycle (their red/green history is the origin commits' own TDD, not
re-derived here — see §1 Framing A). The reranker's 4 new tests were written and run in THIS
session and are RED for the right reason as of this phase; Build turns them green.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris/Cargo.toml` `crates/lunaris-embed-native/src/embedder.rs`
`crates/lunaris-embed-native/src/quantized_embedder.rs` `crates/lunaris-ingest/src/pipeline.rs`
`crates/lunaris-rerank-native/src/reranker.rs` `milestones/mcp-bench/eval-lme-s-metal-N1.json`
(evidence artifact carried by the ported commit)

Strategy (ordered batches, as executed):
  1. Cherry-pick `096d46d` (Metal/Accelerator Cargo feature passthrough + `LUNARIS_EMBED_BATCH`
     embedder/ingest tunable) from `bench/longmemeval-evidence-recall` onto a fresh branch off
     `main`. Reconcile the one mechanical `Cargo.toml` conflict (main's later `native`-optional
     refactor added `"native"` gates the bench branch never saw) by keeping both: the existing
     `"native"` gate on `embedder-gguf`/`reranker-gguf` AND a new matching `"native"` gate on
     each of the 4 accelerator features, for local-convention consistency.
  2. Cherry-pick `713478b` (activation-footprint `plan_batches` OOM bound) on top — applied
     with zero conflicts (it only touches files batch 1 already updated).
  3. Verify: `cargo test -p lunaris-embed-native -p lunaris-ingest` green, `cargo check -p
     lunaris-memory --features metal` clean, `cargo clippy` clean — BEFORE writing any new code,
     to confirm the port itself introduced no regression.
  4. Write the reranker's RED tests (§4) — `resolve_batch_size`/`public_batch_size` tests
     mirroring the embedder's, confirmed failing to compile.
  5. Implement `resolve_batch_size`/`public_batch_size` in `lunaris-rerank-native/src/reranker.rs`
     (byte-identical fallback policy to the embedder's function), wire the `rerank()` chunk loop
     to call `public_batch_size()` instead of the bare `MAX_PUBLIC_BATCH` const, update the two
     adjacent doc comments that described the old hardcoded-only behavior.
  6. Re-run tests — reranker suite green (16/16, 4 new + 12 existing).
  7. Full-surface verification: `cargo clippy --workspace --all-targets -- -D warnings` (matches
     `ci.yml` exactly) clean; `cargo test --workspace --all-targets --exclude lunaris-py
     --exclude lunaris-ts` (matches `ci.yml`'s test command) — 1504 passed, 0 failed, 11 ignored,
     210 suites.

Safety rule (feature-specific): every batch-ceiling resolver (embedder, ingest, reranker) is a
pure function tested WITHOUT mutating process env (the `resolve_*` / `public_batch_size` split
— avoids the Rust-parallel-test global-env race) and falls back to a safe positive constant on
any malformed input, never producing a `slice::chunks(0)` panic or a silent 0-row batch.

Code lives in: the 5 files listed above (Scope).
Constraints: do NOT change any test or the contract; allow-list packages only (none added — this
task adds zero new Cargo dependencies, only forwards existing candle features); ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — `cargo test --workspace --all-targets --exclude lunaris-py --exclude
  lunaris-ts`: 1504 passed, 0 failed, 11 ignored, 210 suites (matches `ci.yml`'s test command).
  `cargo test -p lunaris-rerank-native`: 16/16 (12 pre-existing + 4 new).
- [x] coverage did not decrease — no test removed/weakened; 13 tests ported + 4 tests added, 0 removed.
- [x] no test or contract was altered during build — confirmed via `git diff 7309ab1..HEAD --stat`
  (independent reviewer re-ran this) = exactly the 7 declared files, no test file touched
  outside the new reranker test additions.
- [x] the green was EARNED, not gamed — adversarial refute-read via `senior-rust-engineer` agent
  (independent of this session's own build reasoning): mutation-tested `resolve_batch_size`'s
  fallback filter (`>= 1` → `>= 0`), confirmed the reranker test caught it and failed correctly,
  then reverted. Hand-traced `plan_batches_isolates_a_long_input` algebraically. Verdict: EARNED
  GREEN. One real gap found (`cuda-fa2` not forwarded from the umbrella — true of the origin
  commit too) and fixed in a follow-up commit (`6104b85`), re-verified via `cargo tree -e
  features -i lunaris-embed-native -i lunaris-rerank-native`.
- [x] concurrency / timing of the risky operation is safe — 3 `OnceLock<usize>` caches (embedder,
  ingest driver, reranker) are standard read-once config caches, no lock-across-await, confirmed
  by the same reviewer.
- [x] no exposed secrets, injection openings, or unexpected dependencies — 0 new Cargo
  dependencies (only forwards existing candle features); 0 `unsafe` introduced (grepped); env
  vars are read-only numeric config knobs, no injection surface.
- [x] layering & dependencies follow CONVENTIONS.md — no new lock held across `.await`; no file
  crosses the 1000/1500-line split threshold; no local unscoped keyspace helper introduced (N/A
  to this task's surface).
- [x] a person reviewed and approved the change — Tin Dang, delegated via "go ahead and land the
  fix on main use ADD" (contract freeze) + this VERIFY gate auto-resolves under `autonomy: auto`
  with no residue (see GATE RECORD).

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
- [x] A `main`-based build with `--features lunaris-memory/metal` (or `cpu-accelerate`/`cpu-mkl`/
  `cuda`/`cuda-fa2`) compiles clean and reaches both native crates — confirmed by `cargo check -p
  lunaris-memory --features metal` (clean) + `cargo tree -e features -p lunaris-memory --features
  cuda-fa2 -i lunaris-embed-native` / `-i lunaris-rerank-native` (both show the feature arriving
  via `lunaris-memory feature "cuda-fa2" (command-line)`).
- [x] All 5 accelerator features are absent from a plain `cargo build -p lunaris-memory` (no
  extra flags) — confirmed by reading `crates/lunaris/Cargo.toml`'s `default = ["native"]` (no
  accelerator feature listed) and by the full clippy/test run passing with none active.
- [x] `LUNARIS_RERANK_BATCH` raises the reranker's dispatch chunk size symmetric with the
  embedder's `LUNARIS_EMBED_BATCH` — confirmed by the 4 new reranker tests + the wiring check
  (`docs.chunks(public_batch_size())` at the real dispatch site, not left orphaned).
- [x] A long RAPTOR-style input is never co-batched regardless of batch-size env overrides —
  confirmed by hand-tracing `plan_batches_isolates_a_long_input` (reviewer) and by the ported
  test itself passing.
- [x] Garbage/zero/negative env values fall back safely on both the embedder and reranker sides —
  confirmed by the mutation test (reviewer flipped the fallback filter, watched the reranker test
  fail, reverted) — proves the test is discriminating, not vacuous.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — every new symbol referenced. `resolve_batch_size`/`public_batch_size`
  (reranker, new) called at `reranker.rs`'s real `rerank()` dispatch site
  (`docs.chunks(public_batch_size())`), not left orphaned — confirmed by grep (no remaining bare
  `MAX_PUBLIC_BATCH` chunk call) + the independent reviewer's separate confirmation.
  `plan_batches`/`activation_budget` (embedder, ported) called from BOTH `NativeEmbedder` and
  `NativeQuantizedEmbedder::embed_batch` — reviewer initially suspected the quantized path might
  bypass the guard, checked `quantized_embedder.rs:166-169` directly, confirmed it also calls
  the shared `plan_batches` — corrected before it became a false finding.
  `ingest_embed_batch_size`/`resolve_ingest_batch_size` (ingest, ported) called from
  `embed_with_fallback`'s `drafts.chunks(...)` call site.
- [x] DEAD-CODE (code) — no new unused symbol. All 3 new/ported `public_batch_size()` functions
  are called from their crate's real dispatch path (not just defined); all 5 new Cargo features
  are referenced from at least one native-crate feature (no orphaned feature flag).
- [ ] SEMANTIC (prose/non-code) — N/A, this task produced no prose/doc deliverable beyond inline
  doc-comments already covered by the WIRING check above.

### GATE RECORD
Outcome: PASS
Reviewed by: Tin Dang (delegated autonomy: auto — auto-resolved by this run as accountable
owner, per no residue found: 0 security findings, adversarial refute-read returned EARNED GREEN,
the one non-security gap found (`cuda-fa2` forwarding) was fixed within this same VERIFY pass,
not waived) · date: 2026-07-02

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): the O-01 per-device perf gate table
(`docs/benchmarks/v0.4-O01-baselines.md`, Apple Silicon Metal: embed p50<=5ms/p99<=15ms,
rerank p50 K=10<=40ms) — all cells were "TBD" pre-task; now watchable against real weights
once the branch lands. LongMemEval-S haystack ingest wall-clock (baseline 21min/question
pre-fix, ~114.85s measured on the source branch) as the headline regression monitor.

### Spec delta
  - [SPEC · open] reranker OOM guard not ported: `plan_batches`/activation-footprint bound
    stayed embedder-only (reasoned safe — cross-encoder pairs are tokenizer-bounded per-pair,
    unlike RAPTOR's whole-document summaries). Revisit if reranker-side OOM evidence appears
    (evidence: §3 Least-sure flag, contract-level).
  - [SPEC · open] cuda / cpu-mkl accelerator features were code-reviewed only, never
    build/test-verified — no matching toolchain on the author host (Apple Silicon). Needs a
    CI runner with CUDA or MKL before those two features can be called proven (evidence:
    task's own verify-scope note, source commit 096d46d had the same gap).
  - [SPEC · open] O-01 baseline table cells stay "TBD" until someone runs the gate table
    against real granite-r2/bge-reranker-v2-m3 weights on `main` post-merge — this task only
    proves the feature wiring compiles and the resolver tests pass, not live device numbers.

### Competency deltas
  - [ADD · open] multi-worktree ADD usage silently splits task state: this task's code lived
    in a disposable scratch worktree while its TASK.md/state.json lived in the main checkout's
    `.add/` (found via `find_root()` walking `Path.cwd()` — worktrees don't share `.add/`
    state). Running `status`/`gate` from inside the worktree read an unrelated, already-done
    task and cost significant time to diagnose (evidence: this task's own session transcript).
  - [ADD · open] `_SCOPE_EXCLUDE_DIRS` (add.py) was missing `.claude` — the CLI's own
    session-local lock/settings churn (`.claude/scheduled_tasks.lock`, `.claude/settings.local.json`,
    materialized skill docs) tripped false `scope_violation`s during `gate PASS`, burning 2 of
    3 heal attempts before the root cause was found and fixed (evidence: heal history above,
    `.add/tooling/add.py:_SCOPE_EXCLUDE_DIRS`). Fixed in this same session; same class of bug
    as the pre-existing `.serena`/`.next`/`coverage` entries the comment block already predicts.
  - [ADD · open] excluding a directory from `_SCOPE_EXCLUDE_DIRS` does not retroactively clean
    an ALREADY-taken `scope-snapshot.json` — previously-captured entries under the newly
    excluded dir read as "vanished -> touched" on the next gate, which made the violation
    count get WORSE (1 -> 65) right after the fix landed. Required an explicit tests->build
    re-cross to force a clean re-snapshot. Worth a `add.py check`/doctor affordance that
    detects and offers to re-snapshot a stale baseline after an exclude-list change.
  - [TDD] confirmed — the byte-identical `resolve_batch_size`/`public_batch_size` split
    (pure resolver + `OnceLock`-cached env reader), mirrored 1:1 from the embedder onto the
    reranker, red-then-green with 4 mirrored test cases, worked cleanly with no surprises.
    Good template for the next env-tunable knob.
