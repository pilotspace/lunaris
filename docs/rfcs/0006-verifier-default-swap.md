# RFC 0006 — Verifier default swap: Gemma 3 27B → Gemma 3 270M

| Field        | Value                                          |
|--------------|------------------------------------------------|
| Status       | **Draft** (2026-05-11)                         |
| Author       | Tin Dang                                       |
| Created      | 2026-05-11                                     |
| Target       | Lunaris **v0.2.x** OSS                         |
| Supersedes   | —                                              |
| Related      | `tmp/lunaris-ship-to-product-v2.md` §3 (Phase 21, Phase 24), RFC 0001 (Scope newtype), `crates/lunaris-verify/src/candle_gemma3_27b.rs`, CHANGELOG v0.2.0 / v0.2.1 |

---

## 1. Summary

Swap the **default** Lunaris Verifier backend from **Gemma 3 27B IT** (≈54 GB
RAM, ≈16 GB disk weights, current `candle` default per
`crates/lunaris-verify/src/candle_gemma3_27b.rs:326`) to **Gemma 3 270M**
(≈540 MB RAM, ≈540 MB weights). The 27B path is retained verbatim and stays
opt-in behind a new `--features verify-large` flag. A future
`--features verify-cloud` selects a hosted-API verifier and is explicitly
deferred. The verifier itself remains **default-OFF** per blueprint §5.1
(D-02); this RFC only changes which backend `default_verifier` constructs
**once a caller enables the pipeline**.

The change is a **behavioral**, not API-level, breaking change. The
`Verifier` trait surface, `VerifyDecision` type, and `VerifierBackend` enum
are unchanged. The memory floor for the laptop quickstart drops from ~54 GB
to ~540 MB, putting the entire Lunaris quickstart (Postgres + Lunaris +
extractor + verifier + embedder) inside the **2 GB RAM floor** mandated by
Phase 21's exit gate.

---

## 2. Motivation

### 2.1 Today (v0.2.1)

Verified against `crates/lunaris-verify/src/candle_gemma3_27b.rs:1-113`,
`crates/lunaris-verify/Cargo.toml:24-32`, and
`tmp/lunaris-ship-to-product-v2.md:30`:

- The `candle` feature on `lunaris-verify` selects `CandleGemma3_27B`. The
  module rustdoc explicitly calls out "27B takes ~16 GB disk / ~24 GB RAM"
  in the cache-miss error message — but that 24 GB number is the *quantized*
  load; FP16 inflates to ~54 GB resident at forward time.
- The 27B per-call timeout is **1500 ms batch / 800 ms per-chunk** because
  the model is 7–10× slower than the 4B extractor on CPU. That is the
  verifier slow-path budget; it is not a property we want to advertise on a
  laptop quickstart.
- A user running `cargo add lunaris --features candle` on a 16 GB MacBook
  Pro today gets **OOM-killed during model load**, before a single episode
  is ingested. The `default_verifier` factory has no recovery path — the
  candle feature implies the 27B model.

### 2.2 Why now

1. **"Lunaris on a laptop" is the v0.2 OSS promise.** Phase 21's exit gate
   in the master plan is *"running the v0.2 quickstart end-to-end on a 16 GB
   Mac uses ≤2 GB RAM."* That is unattainable while the default Verifier
   wants 54 GB.
2. **Verification is a binary acceptance decision.** The verifier reads one
   `NeedsReviewItem` and emits a `VerifyDecision` of shape `{winner_id,
   loser_id, reason}` (see `parse_decision_json` in
   `crates/lunaris-verify/src/lib.rs:160-204`). The model does not need to
   *extract* primitives — the extractor already did that. The verifier picks
   one of two ulids and writes a short reason. 270M has ample headroom for
   this constrained generation task.
3. **The extractor is the model that needs to be big.** Free-form
   primitive extraction (entity / relation / fact JSON from raw prose) is
   the load-bearing quality call, and that stays at Gemma 3 4B by default
   (`crates/lunaris-extract/src/candle_gemma3.rs:486`). 270M would
   underperform there; we are not touching the extractor.
4. **OSS adopters churn at first OOM.** A 30+ GB first-impression cost is a
   silent funnel killer — the user uninstalls before they file an issue.
   Every Mem0 / Zep / Cognee comparison thread that mentions "fits on my
   laptop" is one we lose today.

### 2.3 What this RFC does NOT change

- The `Verifier` trait, `VerifyDecision`, `VerifierBackend` enum, and the
  worker's MVCC supersede flow (one `atomic_write` per accepted decision
  per D-11) are unchanged.
- The verifier remains **default-OFF**. `default_verifier` only constructs
  the backend; the worker is not spawned until
  `VerifierPipelineHandle::enable()` is called (Plan 04-04). This RFC
  preserves that contract.
- The 27B implementation file (`candle_gemma3_27b.rs`) is **not deleted**.
  It moves from "default candle backend" to "opt-in heavy backend" via the
  feature matrix in §3.2.
- The `OllamaVerifier` and `CloudApiVerifier` backends are unchanged.

---

## 3. Design

### 3.1 New backend: `CandleGemma3_270M`

Add `crates/lunaris-verify/src/candle_gemma3_270m.rs`. It mirrors the 27B
file structurally — same `Verifier` trait impl, same `arbitration_prompt`
template (shared module-level helper in `lunaris-verify/src/lib.rs:121`),
same `parse_decision_json` JSON-tolerant decoder — with only these
divergences:

```rust
// crates/lunaris-verify/src/candle_gemma3_270m.rs

/// Default sub-directory under the user's cache root.
const DEFAULT_CACHE_SUBDIR: &str = "lunaris/models/gemma-3-270m-it";

/// Default per-batch timeout — 270M is ~100x faster than 27B on CPU, so
/// the budget drops from 27B's 1500 ms to a tight 150 ms (matches the
/// extractor's per-batch budget, since model size is comparable to the
/// 300M embedder).
pub const DEFAULT_BATCH_TIMEOUT_MS: u64 = 150;

/// Default per-chunk fallback timeout.
pub const DEFAULT_PER_CHUNK_TIMEOUT_MS: u64 = 80;

/// Default max-new-tokens cap. Arbitration reasons stay short.
pub const DEFAULT_MAX_NEW_TOKENS: usize = 256;

#[derive(Clone, Debug)]
#[allow(non_camel_case_types)]
pub struct CandleGemma3_270MOpts {
    pub model_path: Option<PathBuf>,
    pub device: Device,
    pub batch_timeout_ms: u64,
    pub per_chunk_timeout_ms: u64,
    pub max_new_tokens: usize,
}

impl Default for CandleGemma3_270MOpts { /* mirrors 27B Default impl */ }

#[allow(non_camel_case_types)]
pub struct CandleGemma3_270M { inner: Arc<Inner> }

impl CandleGemma3_270M {
    pub async fn new(opts: CandleGemma3_270MOpts) -> Result<Self, LunarisError> { /* ... */ }
}

#[async_trait]
impl Verifier for CandleGemma3_270M {
    async fn verify(&self, item: NeedsReviewItem) -> Result<VerifyDecision, LunarisError> {
        // identical structure to 27B::verify: spawn_blocking + tokio::time::timeout,
        // parking_lot::Mutex<Gemma3Model> taken inside spawn_blocking (never
        // across .await — matches CLAUDE.md lock-discipline rule).
    }
}
```

The model uses the same `candle_transformers::models::gemma3::Model` typed
backbone — Gemma 3 270M shares the architecture family; only the config and
weights differ. No new candle dependency surface.

### 3.2 Feature flag matrix

Update `crates/lunaris-verify/Cargo.toml`:

```toml
[features]
default      = ["verify-small"]                     # NEW: small is the default
verify-small = ["dep:candle-core", "dep:candle-nn", "dep:candle-transformers", "dep:tokenizers", "dep:dirs"]
verify-large = ["verify-small"]                     # additive — large pulls small's deps + 27B module
verify-cloud = ["dep:reqwest"]                      # alias of today's `cloud-api`; deferred selection logic

# Back-compat aliases (one minor release of overlap):
candle    = ["verify-small"]                        # deprecated alias
ollama    = ["dep:reqwest"]                         # unchanged
cloud-api = ["verify-cloud"]                        # deprecated alias

verifier-it = []                                    # unchanged
```

Module gating in `lunaris-verify/src/lib.rs`:

```rust
#[cfg(feature = "verify-small")]
pub mod candle_gemma3_270m;
#[cfg(feature = "verify-large")]
pub mod candle_gemma3_27b;

#[cfg(feature = "verify-small")]
pub use candle_gemma3_270m::{CandleGemma3_270M, CandleGemma3_270MOpts};
#[cfg(feature = "verify-large")]
pub use candle_gemma3_27b::{CandleGemma3_27B, CandleGemma3_27BOpts};
```

`default_verifier` (Plan 04-04 factory) resolves to the smallest enabled
backend, in this priority order:

1. `verify-large` → `CandleGemma3_27B` (operator opted into the heavy
   path explicitly)
2. `verify-small` → `CandleGemma3_270M` (the new default)
3. else `NoopVerifier` (pipeline disabled — unchanged)

**No runtime auto-fallback.** The selection is resolved at construction
time. A 270M backend that fails to load returns `LunarisError`; the caller
decides whether to fall back to `NoopVerifier`, swap in a cloud backend,
or hard-fail. Silent quality drift (a 270M result masquerading as a 27B
result in telemetry) is the failure mode we reject — see §4 telemetry.

### 3.3 What `verify-cloud` ships

`verify-cloud` is the renamed `cloud-api` feature. The selection logic
(env-var-driven `LUNARIS_VERIFY_PROVIDER` between Anthropic / OpenAI /
Gemini) is unchanged. This RFC's only `verify-cloud` deliverable is the
feature alias and CHANGELOG entry; the hosted-API verifier already exists.

---

## 4. Quality gate

This is the load-bearing section. A default swap that quietly halves
verifier acceptance quality is worse than the 54 GB floor we are trying to
escape.

### 4.1 Gate dataset — `data/verify-gate/v0.2.x.jsonl`

Hand-curated **100-item** verification benchmark:

- **75 items** sampled from the LongMemEval-S contradiction split. Each
  item is a `NeedsReviewItem::{Fact, Relation, Entity}` synthesized from
  two contradicting episodes with ground-truth winner/loser ulids
  established by the LongMemEval gold labels.
- **25 items** hand-crafted edge cases covering:
  - **Negation** (10): "Alice is married to Bob" vs "Alice is not married
    to Bob" — verifier must pick the version whose `t_ref` is later.
  - **Temporal logic** (10): a fact `valid_until=2024-01-01` superseded by
    a fact `valid_from=2024-06-01` — verifier must pick the latter.
  - **Contradiction with shared subject** (5): two `Relation` rows with
    same `src` + `kind` but different `dst`, with one having stale
    `bt.sys`.

Each row schema:

```json
{
  "id": "vg-001",
  "category": "negation|temporal|contradiction|longmemeval",
  "needs_review": { /* serialized NeedsReviewItem */ },
  "gold": { "winner_id": "01HX...", "loser_id": "01HY...", "abstain_ok": false }
}
```

### 4.2 Acceptance criteria

The gate runs both 27B and 270M against every item and computes:

- **Baseline-disagreement rate** (the primary metric): of the items where
  27B produces a non-deferred decision, what fraction does 270M produce a
  *different* non-deferred decision for? **Target: ≤ 5 %.** (i.e. when
  the 27B says "verified", 270M agrees ≥ 95 % of the time.)
- **Defer-rate delta**: 270M may defer more often than 27B. **Target: ≤
  +15 percentage points.** A defer is not a wrong answer — the worker
  treats it as "skip the supersede" (per `worker.rs`), so a higher defer
  rate degrades recall correctness gradually, not catastrophically.
- **Hard-error rate**: any `LunarisError` from the 270M backend across the
  100 items. **Target: 0**.

These three numbers gate the v0.2.x release that flips the default. They
land in `docs/benchmarks/v0.2.x/verify-gate.json` (Phase 24 publishes the
head-to-head).

### 4.3 Telemetry — production drift alarm

The worker (`crates/lunaris-verify/src/worker.rs`) emits a new counter via
the `metrics` crate on every decision:

```rust
metrics::counter!(
    "lunaris_verify_decisions_total",
    "backend" => backend.as_str(),     // "candle-270m" | "candle-27b" | "ollama" | "cloud-anthropic" | ...
    "outcome" => outcome.as_str(),     // "arbitrate" | "deferred" | "error"
).increment(1);
```

A second counter, `lunaris_verify_baseline_divergence_total`, is incremented
only in **shadow-eval mode** — when an operator runs `verify-large` *and*
`verify-small` simultaneously with the small backend wired as the
production decision-maker and the large backend as a shadow consumer. Any
decision where the two backends produce different non-deferred
`VerifyDecision::winner_id` increments the counter.

Operator alert: page at **>1 % shadow divergence over a 1 h window**. The
runbook entry is `docs/runbooks/verifier-divergence.md` and points the
operator at `--features verify-large` as the supported escalation. Shadow
eval is opt-in; production deployments without the heavy backend pay zero
runtime cost for this telemetry.

### 4.4 Rollback plan

The rollback is **a single feature flag**:

```bash
cargo add lunaris --features verify-large
```

The 27B backend remains a first-class citizen, fully tested in
`verifier-it`, and produces bit-for-bit equivalent decisions to v0.2.0's
`candle` default. Operators whose workload routinely crosses the 5 %
divergence line have a one-line escape hatch; their migration cost is
disk + RAM, not code.

There is **no runtime kill switch** that downgrades from 270M to
`NoopVerifier` — abstain-on-error semantics already absorb transient
backend failures (the worker treats a `LunarisError` from `verify()` as a
deferred decision per Plan 04-01 Task 3).

---

## 5. Migration plan

Single workspace bump to `lunaris@0.2.x` (target `v0.2.2`):

1. **Cargo feature renames** (§3.2). `candle` → `verify-small`,
   `cloud-api` → `verify-cloud`. The deprecated aliases stay for one
   minor release with a `#[deprecated]` re-export so existing
   `Cargo.toml`s keep compiling.
2. **Default backend swap.** Users who pinned `lunaris = "0.2"` and built
   with `--features candle` (now `verify-small`) see the new default at
   `cargo update`. Their verifier still loads — the cache directory
   changes from `gemma-3-27b-it/` to `gemma-3-270m-it/`, so a fresh
   weights download (~540 MB) is triggered.
3. **Release notes** call this out as **BEHAVIORAL change, not API
   change**:
   - The `Verifier` trait, `VerifyDecision`, and worker contract are
     unchanged.
   - The verifier identity at runtime changes; downstream consumers that
     log `VerifyDecision::backend` see `VerifierBackend::Candle270M`
     (new variant — see §7) instead of `VerifierBackend::Candle`.
4. **Migration guide entry** at `docs/migration/0.2.1-to-0.2.2.md`:
   - Operators who want 27B back: add `verify-large` to their feature
     list. No code changes.
   - Operators who want zero verifier (the v0 default-OFF posture
     untouched): no action; the pipeline stays disabled unless they call
     `enable()`.
5. **CHANGELOG entry** under `## v0.2.2` with "Default Verifier swapped
   from Gemma 3 27B to Gemma 3 270M — memory floor 54 GB → 540 MB. Set
   `--features verify-large` to restore the 27B default."

The Postgres / Moon storage schemas, RLS policies, and scope alphabet
(RFC 0001, CHANGELOG v0.2.1) are unaffected.

---

## 6. Rejected alternatives

| Alternative                                    | Rejected because                                                                                                                            |
|------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------|
| Stay on Gemma 3 27B default                    | Violates Phase 21's 2 GB laptop floor; OOMs new OSS adopters at first quickstart. The motivation section.                                   |
| Default to Gemma 3 1B                          | Middle ground (~2 GB) — fits a 4 GB laptop but not Phase 21's "≤2 GB RAM total quickstart" gate. Verification is binary; 270M suffices.    |
| Default to a hosted API (Anthropic / OpenAI)   | Adds an external network dependency, API key requirement, and rate-limit failure mode to the OSS quickstart. Kept as `verify-cloud` opt-in. |
| Runtime auto-fallback 270M → 27B on low confidence | Silent quality drift; `VerifierBackend` tag stops being meaningful in telemetry. Construction-time selection is the discipline.        |
| Drop `candle_gemma3_27b.rs` entirely            | Forfeits the rollback path; some operators (Helios scale, contradiction-heavy workloads) will exceed the 5 % divergence bar.               |
| Quantize 27B (Q4_K_M) to fit in ~16 GB         | Still 16 GB. The 2 GB floor is the goal; quantization is a different axis (could stack with 270M later if quality permits).               |

---

## 7. Compatibility

**Behavioral change only.** The trait surface is unchanged:

- `Verifier::verify(&self, item: NeedsReviewItem) -> Result<VerifyDecision, LunarisError>` — unchanged.
- `Verifier::applies(&self) -> bool` — unchanged.
- `VerifyDecision` shape (`winner_id`, `loser_id`, `reason`, `backend`) — unchanged.

One additive enum change in `lunaris-verify::types`:

```rust
pub enum VerifierBackend {
    Noop,
    Candle,          // 27B — retained, semantics unchanged
    Candle270M,      // NEW — 270M variant; emitted by CandleGemma3_270M
    Ollama,
    Cloud(CloudProvider),
}
```

This is `#[non_exhaustive]`-equivalent in practice (callers match on
`VerifierBackend` only in audit logging, where unknown variants render via
`Debug`). The PyO3 / napi SDKs surface the new variant via the existing
serde-derived JSON path; no SDK regen required beyond a re-cut binding for
the enum.

External adopters who built against `lunaris@0.2.0` or `0.2.1` with
`--features candle` will see:

- A new model download on first verifier construction (~540 MB instead of
  ~16 GB) — *strict improvement*.
- A new `VerifierBackend::Candle270M` value in audit log entries —
  forward-compatible since audit consumers treat unknown backends as
  opaque strings.
- The same `Verifier` API; their construction code is unchanged.

---

## 8. Open questions

1. **Should the 270M weights ship vendored with the crate (~540 MB) or
   download on first use?** Recommendation: **download on first use** with
   SHA-256 checksum pinning in `lunaris-verify/Cargo.toml`. Bundling
   ~540 MB of weights into every `cargo add lunaris` build is hostile to
   docs.rs, crates.io upload limits, and CI caches. The cache-miss error
   message (mirroring the 27B one at
   `crates/lunaris-verify/src/candle_gemma3_27b.rs:39`) gives the user a
   one-line `huggingface-cli download google/gemma-3-270m-it` recovery
   path.
2. **Should `default_verifier` auto-download on cache miss?** No —
   silent multi-GB network calls on `Lunaris::new` are a worse footgun
   than a clear error message. The user explicitly runs the download
   command surfaced in the error.
3. **Does `OllamaVerifier::default_model` need to change too?** Out of
   scope for this RFC; it is set by the operator's Ollama config, not
   Lunaris. We document the recommendation (`gemma3:270m`) in the
   verifier README without changing code defaults.

---

## 9. Verification plan

Two GSD phases gate this RFC:

### Phase 21 — build the gate dataset and 270M backend

- `crates/lunaris-verify/src/candle_gemma3_270m.rs` lands with unit tests
  mirroring the existing 27B test list (`opts_default_resolves_to_cache_subdir`,
  cache-miss error shape, `Verifier` dyn-compat).
- `data/verify-gate/v0.2.x.jsonl` lands with the 100-item dataset and a
  schema validator under `tools/verify-gate/validate.rs`.
- `cargo test -p lunaris-verify --features verify-small,verifier-it
  --test verify_gate` runs the dataset end-to-end against a fresh 270M
  load and asserts the three §4.2 criteria against pinned baselines.

### Phase 24 — reproducible head-to-head benchmark publication

- `make bench-verify` extends Phase 24's `make bench-public` with a
  `verify-gate` run that produces:
  - `docs/benchmarks/v0.2.x/verify-gate.json` — full per-item decisions
    for 27B and 270M plus the divergence table.
  - `docs/benchmarks/v0.2.x/verify-gate.md` — human-readable narrative
    with the §4.2 numbers prominent.
- CI gate: the verify-gate run is **required green** in the release
  workflow before the `v0.2.x` tag is published. A regression that
  pushes divergence above 5 % blocks the release; the operator-facing
  CHANGELOG cannot ship a "swap is safe" message that the gate
  contradicts.

Both phases run in CI before crates.io / PyPI / npm publish. Local
reproducibility uses the standing `scripts/sdk-real-evidence.sh` harness
pattern — no new local-rig requirements beyond the 270M weights download.

---

## 10. Decision log

- **2026-05-11** — Author opened RFC. Selection: 270M (not 1B) as default;
  the 2 GB laptop floor is the inviolable target and 1B blows it. Gemma 3
  family chosen for architecture re-use with the existing
  `candle_transformers::models::gemma3::Model` backbone — no new
  candle dependencies.
- **2026-05-11** — Rejected runtime auto-fallback after considering the
  silent-divergence telemetry hole. Construction-time backend selection
  preserves the `VerifierBackend` audit contract.
- **2026-05-11** — Feature-flag matrix shaped as `verify-small` /
  `verify-large` / `verify-cloud` instead of overloading `candle`. Old
  `candle` / `cloud-api` aliases retained one minor release for
  back-compat.
