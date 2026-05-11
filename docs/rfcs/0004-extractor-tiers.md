# RFC 0004 — `ExtractorTier` typestate enum and laptop-floor default swap

| Field        | Value                                          |
|--------------|------------------------------------------------|
| Status       | **Draft** (2026-05-11)                         |
| Author       | Tin Dang                                       |
| Created      | 2026-05-11                                     |
| Target       | Lunaris **v0.2.x** OSS                         |
| Supersedes   | —                                              |
| Related      | `tmp/lunaris-ship-to-product-v2.md` §3 Phase 21, `tmp/xmem-grounded-findings-and-pickups.md` §2.3, RFC 0001, RFC 0003 (fallback-extractor) |

---

## 1. Summary

Introduce an `ExtractorTier { Tiny, Small, Medium, Large }` typestate enum
in `lunaris-extract` mapped onto Gemma 3 270M / 1B / 4B / Cloud-API
implementations, and flip the OSS default from Gemma 3 **4B → 1B (Small)**
at the v0.2.x line. Tier selection is **static dispatch via a generic
`Extractor` bound** — the umbrella `Lunaris::with_extractor` `Box<dyn>`
escape hatch remains for advanced callers, but the default builder path
monomorphizes at the tier boundary.

This is the second half of the multi-agent-credibility story RFC 0001
started: OSS adopters cannot evaluate Lunaris on a 16 GB laptop while the
default extractor demands a ~4 GB Gemma 3 4B floor. Pro users who already
provisioned the 4B path keep it behind an opt-in feature flag; cloud-API
users get a first-class tier name instead of a `--features cloud-api`
muscle-memory rite.

---

## 2. Motivation

### 2.1 Today (v0.2.0)

Verified against
`crates/lunaris-extract/src/lib.rs:79-95`,
`crates/lunaris-extract/src/candle_gemma3.rs:55-68`, and
`crates/lunaris-extract/Cargo.toml:27-34`:

- The `lunaris-extract` crate ships **one in-tree LLM-backed extractor**:
  `CandleGemma3_4B`. The Cargo default is `["candle"]` and the candle
  feature pulls Gemma 3 4B weights from
  `~/.cache/lunaris/models/gemma-3-4b-it/`.
- The dyn-compatible `Extractor` trait is the only public surface. The
  umbrella `Lunaris::with_extractor(Arc<dyn Extractor>)`
  (`crates/lunaris/src/handle.rs:338`) accepts any backend, but the
  **out-of-the-box experience** loads 4B weights when the graph pipeline
  is enabled.
- `OllamaExtractor` and `CloudApiExtractor` exist behind `ollama` /
  `cloud-api` features. There is **no in-tree 270M or 1B extractor**.
- Tier intent is expressed today by combinations of Cargo features +
  manual `with_extractor` calls + the GraphPipelineHandle toggle. There
  is no single typed entry point that says "I want the laptop tier."

### 2.2 Why now (v0.2.x, not v0.3)

1. **The v0.2 OSS launch story is "16 GB Mac, ≤ 2 GB RAM, recall in 10
   minutes."** The 4B floor breaks that story before the quickstart hits
   `cargo run`. Phase 21 in `tmp/lunaris-ship-to-product-v2.md` makes the
   laptop floor an explicit exit gate; v0.3 is too late.
2. **Tier choice should be a compile-time guarantee, not a runtime
   surprise.** Mem0 / Zep / Cognee all do "auto-detect-the-LLM" magic
   that breaks under air-gapped or cost-sensitive deployment. Lunaris's
   moat is the contract, not the configuration: the chosen tier must be
   inspectable at the type level.
3. **Static dispatch is a published principle.** RFC 0003's
   `FallbackExtractor<E>` is generic-not-`dyn` for exactly the same
   reason; introducing a `Box<dyn Extractor>`-on-hot-path default now
   would contradict the principle we just shipped.
4. **The Verifier 27B → 270M swap (Phase 21 P0) is already landing.** A
   parallel `ExtractorTier` enum lets the two model-tiering stories share
   one mental model and one feature-flag vocabulary.

### 2.3 What we are NOT doing in this RFC

- Removing the `Arc<dyn Extractor>` escape hatch. `with_extractor` stays
  for custom backends (testing doubles, third-party LLMs, the
  `FallbackExtractor<E>` combinator).
- Auto-tier-by-available-RAM. Magic detection is rejected in §6.
- New extractor backends beyond the 270M / 1B / Cloud surface. Ollama
  stays where it is — `OllamaExtractor` is a transport, not a tier.
- A breaking signature change to the `Extractor` trait itself. Tier is a
  selection mechanism above the trait, not a trait amendment.

---

## 3. Design

### 3.1 The `ExtractorTier` enum (`lunaris-extract`)

```rust
/// Compile-time selector for the in-tree extractor backend.
///
/// Each variant maps to one concrete `Extractor` impl. The mapping is
/// **static** — selecting a tier monomorphizes the engine builder against
/// the corresponding impl; there is no runtime `Box<dyn Extractor>` on
/// the ingest hot path.
///
/// # Tier mapping
///
/// | Variant   | Backend                              | RAM floor (informative) |
/// |-----------|--------------------------------------|-------------------------|
/// | `Tiny`    | `CandleGemma3_270M`                  | ~0.4 GB                 |
/// | `Small`   | `CandleGemma3_1B`   *(v0.2 default)* | ~1.2 GB                 |
/// | `Medium`  | `CandleGemma3_4B`   *(v0.1 default)* | ~4.0 GB                 |
/// | `Large`   | `CloudApiExtractor`                  | ~0 GB local             |
///
/// Numbers are informative ballparks pending Phase 24 bench-gate
/// (`make bench-public`). See §4.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ExtractorTier {
    Tiny,
    Small,
    Medium,
    Large,
}

impl ExtractorTier {
    /// The tier the *compiled* binary defaults to, derived from
    /// active Cargo features. See §3.3 for the precedence table.
    pub const fn compile_time_default() -> Self { /* cfg-resolved */ }

    /// Stable string name (`"tiny"`, `"small"`, `"medium"`, `"large"`).
    /// Used by `tracing` spans and the `Lunaris::extractor_tier()`
    /// inspection accessor.
    pub const fn as_str(self) -> &'static str { /* ... */ }
}
```

The enum is `Copy` — it is a tag, not a handle. The handle is the
generic `E: Extractor` instance bound at builder time.

### 3.2 Generic dispatch on the umbrella builder

```rust
// crates/lunaris/src/handle.rs

pub struct LunarisBuilder<E: Extractor = DefaultExtractor> {
    // ... existing fields (storage port, embedder, etc.) ...
    extractor: E,
    tier: ExtractorTier,
}

impl LunarisBuilder<DefaultExtractor> {
    /// Construct a builder using the compile-time-default extractor
    /// (`ExtractorTier::compile_time_default()`).
    pub fn new() -> Self { /* ... */ }
}

impl<E: Extractor> LunarisBuilder<E> {
    /// Swap in a different in-tree tier. Returns a fresh builder with the
    /// monomorphized type parameter — call sites that go through this
    /// method preserve static dispatch.
    pub fn extractor_tier(self, tier: ExtractorTier)
        -> LunarisBuilder<TierExtractor>
    { /* match tier => concrete impl */ }

    /// Plug a custom extractor (the existing `Arc<dyn Extractor>` escape
    /// hatch). Use this for `FallbackExtractor<E>`, test doubles, or
    /// third-party backends.
    pub fn extractor<E2: Extractor>(self, ext: E2) -> LunarisBuilder<E2>
    { /* ... */ }

    pub fn build(self) -> Lunaris<E> { /* ... */ }
}
```

`DefaultExtractor` is a `cfg`-resolved type alias (see §3.3) so the
"no-explicit-tier" call site monomorphizes to the right impl without a
`Box`.

`TierExtractor` is a small enum-dispatch newtype (think `enum_dispatch`
or a hand-rolled `match`) wrapping the four concrete impls. Inside
`extractor_tier()` the runtime branch happens **once**, at builder time;
the resulting `Lunaris<TierExtractor>` carries the monomorphized type
forward. The hot path (`Lunaris::ingest`) sees a single concrete
`E::extract` call — no v-table lookup per chunk.

**Why not pure type-level dispatch (zero enum)?** A pure-types design
would force the call site
`Lunaris::builder().extractor_tier::<TierTiny>()` — fine for a library
author, hostile to a beginner. The single enum-dispatch newtype is the
ergonomic compromise: one runtime branch at builder time, zero on the
hot path.

### 3.3 Default tier per Cargo-feature combination

| Active features                                              | `compile_time_default()` | Concrete impl             |
|--------------------------------------------------------------|--------------------------|---------------------------|
| *(no extractor features)*                                    | `Small`                  | `CandleGemma3_1B`         |
| `extract-tiny`                                               | `Tiny`                   | `CandleGemma3_270M`       |
| `extract-small` *(explicit form of the default)*             | `Small`                  | `CandleGemma3_1B`         |
| `extract-medium`                                             | `Medium`                 | `CandleGemma3_4B`         |
| `extract-large` *(implies `cloud-api`)*                      | `Large`                  | `CloudApiExtractor`       |
| Multiple feature flags                                       | Highest-priority wins\*  | per row above             |

\* Precedence (highest first): `extract-large > extract-medium > extract-tiny > extract-small`. Conflicts emit a `compile_error!` only
when two **exclusive** flags are set (e.g. `extract-tiny` +
`extract-medium`); the additive case `extract-small` + `extract-large`
is allowed because callers may want both tiers available at runtime via
`extractor_tier(...)`.

**Cargo.toml diff (informative):**

```toml
[features]
default        = ["candle", "extract-small"]
candle         = [ "dep:candle-core", "dep:candle-nn",
                   "dep:candle-transformers", "dep:tokenizers",
                   "dep:dirs", "dep:parking_lot" ]
# New tier features. Each implies `candle` so the in-tree Gemma 3 impls
# compile; `extract-large` additionally implies `cloud-api`.
extract-tiny    = ["candle"]
extract-small   = ["candle"]
extract-medium  = ["candle"]
extract-large   = ["cloud-api"]
ollama          = ["dep:reqwest"]
cloud-api       = ["dep:reqwest"]
```

`default = ["candle", "extract-small"]` is the OSS laptop story:
**Gemma 3 1B is what `cargo add lunaris` gets you.**

### 3.4 Builder ergonomics (call-site snapshot)

```rust
// Laptop default — Small / 1B, ≤ ~1.2 GB RAM
let lunaris = Lunaris::builder()
    .storage(moon_storage)
    .build()
    .await?;

// Pro tier — explicit Medium / 4B (v0.1 default; opt-in at v0.2.x)
let lunaris = Lunaris::builder()
    .storage(pg_storage)
    .extractor_tier(ExtractorTier::Medium)
    .build()
    .await?;

// Cloud — Large; LUNARIS_EXTRACT_PROVIDER selects Anthropic/OpenAI/Gemini
let lunaris = Lunaris::builder()
    .storage(pg_storage)
    .extractor_tier(ExtractorTier::Large)
    .build()
    .await?;

// Escape hatch — fallback combinator (RFC 0003), still type-checked
let fallback = FallbackExtractor::new()
    .push(CandleGemma3_1B::from_default_cache()?)
    .push(CloudApiExtractor::from_env()?);
let lunaris = Lunaris::builder()
    .storage(pg_storage)
    .extractor(fallback)
    .build()
    .await?;
```

### 3.5 Inspection accessor

```rust
impl<E: Extractor> Lunaris<E> {
    /// Returns the tier this engine was built with. Returns `None` if
    /// the caller plugged a custom extractor via `.extractor(...)`.
    pub fn extractor_tier(&self) -> Option<ExtractorTier> { /* ... */ }
}
```

Useful for the bench harness, the `/healthz` route in `lunaris-server`,
and the migration deprecation warning (§5).

---

## 4. Quality vs latency table *(informative — Phase 24 bench-gated)*

| Tier     | Backend                | ER-F1 (target) | Ingest p50 (one 1.5k-token chunk) | RAM floor | Notes                                       |
|----------|------------------------|----------------|-----------------------------------|-----------|---------------------------------------------|
| `Tiny`   | Gemma 3 270M           | ≥ 0.55         | ~120 ms (CPU) / ~40 ms (Metal)    | ~0.4 GB   | Laptop survival mode; lossy on relations.   |
| `Small`  | Gemma 3 1B *(default)* | ≥ 0.70         | ~250 ms (CPU) / ~80 ms (Metal)    | ~1.2 GB   | OSS quickstart target.                      |
| `Medium` | Gemma 3 4B             | ≥ 0.82         | ~600 ms (CPU) / ~180 ms (Metal)   | ~4.0 GB   | Today's default; opt-in at v0.2.x.          |
| `Large`  | Cloud-API              | ≥ 0.85         | ~250–900 ms (network-bound)       | ~0 GB     | Cost-bound; subject to provider SLO.        |

All four columns are **informative ballparks**. The Phase 24
benchmark harness (`make bench-public` in
`tmp/lunaris-ship-to-product-v2.md`) gates the published numbers. The
RFC's contract is:

- **`ExtractorTier::Small` MUST hit ER-F1 ≥ 0.70 on the LongMemEval-S
  subset.** If it does not at bench close-out, the v0.2.x default
  remains `Medium` and this RFC ships with the enum + tier
  infrastructure only.
- **`ExtractorTier::Small` MUST stay under 1.5 GB RAM floor** on the
  quickstart fixture. If it does not, the default falls back to `Tiny`
  and the migration guide is amended.

---

## 5. Migration plan

### 5.1 v0.2.x — flip the default, keep 4B reachable

- Cargo default becomes `default = ["candle", "extract-small"]`. A user
  who runs `cargo update -p lunaris` from v0.2.0 to v0.2.x gets the 1B
  backend on the next ingest with a graph pipeline enabled.
- v0.1-style call sites that constructed the engine with no explicit
  tier (`Lunaris::builder().build()`) now monomorphize against
  `CandleGemma3_1B`. **This is the breaking-behaviour line.** It is not
  a breaking API change.
- A `tracing::warn!` fires at builder time if the compile-time tier
  diverges from a non-`None` `LUNARIS_EXTRACTOR_TIER` env override —
  surfaces the case where ops people pin one tier in `Cargo.toml` and a
  deploy script tries to override at runtime.

### 5.2 v0.3 — remove `Medium` as the default-default

- `extract-medium` remains a supported feature; the default Cargo
  feature set no longer references it under any combination.
- Migration guide (`docs/migration/0.2-to-0.3.md`) ships the one-liner:
  ```diff
  - lunaris = "0.2"
  + lunaris = { version = "0.3", features = ["extract-medium"] }
  ```
  for users who want the v0.1 behaviour.

### 5.3 No silent model downloads

- Each tier's `from_default_cache()` constructor returns the same
  actionable error today's `CandleGemma3_4B` does (the
  `gemma-3-1b-it weights missing at PATH — run huggingface-cli download …`
  pattern). Plan 03-03's `NoopExtractor` fallback applies uniformly.

---

## 6. Rejected alternatives

| Alternative                                              | Rejected because                                                                                                                       |
|----------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------|
| `Box<dyn Extractor>` on the default hot path             | Kills static dispatch on every chunk's `extract` call. Contradicts RFC 0003's stated principle (`FallbackExtractor<E>` generic-not-dyn) and the umbrella `Lunaris<E>` design. |
| Single Gemma 3 1B without an enum                        | Loses the laptop / pro / cloud story at the type level. Bench-gating becomes ad-hoc; no inspection accessor; no migration path for users who want 4B back. |
| Runtime auto-tier from available RAM                     | Magic, undebuggable, varies by host. A 16 GB Mac running other apps would silently downgrade mid-session. No compile-time guarantee. |
| Type-level dispatch only (`extractor_tier::<TierTiny>()`)| Ergonomically hostile to the laptop adopter who just wants `Lunaris::builder().build()`. The single enum-dispatch newtype is a one-time runtime branch at builder time — zero on the hot path. |
| Per-tier crate (`lunaris-extract-tiny`, …)               | Doubles the workspace surface for ~200 LoC per tier. Revisit only if `Large` migrates to a feature crate (see §8 open question). |
| Keep 4B default; document the 16 GB Mac as "not supported"| Breaks the v0.2 OSS exit gate (`tmp/lunaris-ship-to-product-v2.md` Phase 21). Cedes the laptop story to Mem0. |

---

## 7. Compatibility

**Source-compatible at the API level.** The `Extractor` trait,
`with_extractor`, `Lunaris::builder()`, and `ExtractorTier` are
additive. Existing callers of `Lunaris::builder().build()` recompile
without code changes.

**Behaviour-breaking at the default-extractor boundary.** A caller who:

1. Pinned `lunaris = "0.2"` (no version pin to 0.2.0), and
2. Constructed `Lunaris` with no explicit `extractor_tier(...)` or
   `with_extractor(...)`, and
3. Relied on the implicit 4B model load

…will, on the next `cargo update`, get the 1B model instead. The
one-liner upgrade is:

```diff
- lunaris = "0.2"
+ lunaris = { version = "0.2", features = ["extract-medium"] }
```

The migration guide (`docs/migration/0.2-to-0.2.x.md`) calls this out
explicitly. CHANGELOG carries a top-of-file "Behaviour change" banner.

**No on-the-wire change.** Extractions produced by `Small` are
schema-identical to those produced by `Medium`; the validator (D-08)
treats them uniformly. Recall p50 / atomic-write contracts are
untouched.

---

## 8. Open questions

- **Should `Large` live in `lunaris-extract` behind `cloud-api`, or
  migrate to a sibling crate `lunaris-extract-cloud`?** Arguments for a
  separate crate: cloud-API providers churn faster than the candle
  stack; the `reqwest` dep weight is wasted for laptop adopters; the
  `LUNARIS_EXTRACT_PROVIDER` env-resolution surface is large enough to
  earn its own crate. Arguments against: doubles the publish surface
  for v0.2.x; users discovering `lunaris-extract-cloud` separately is
  worse UX than `--features extract-large`. **Tentative answer:** keep
  in-tree behind `extract-large` for v0.2.x; revisit at v0.3 when the
  bench harness has data on cloud-API drift.
- **Should `ExtractorTier::compile_time_default()` be a `const fn` or a
  runtime function?** `const fn` is the obvious choice for inspection,
  but the cfg-resolution shape may force runtime — confirm during
  implementation.
- **Should the deprecation warning in §5.1 fire on every `builder()`
  call or only once per process?** Once per process via
  `std::sync::Once` is the sane default; revisit if logs are noisy in
  the wild.
- **Does `TierExtractor` warrant the `enum_dispatch` proc-macro, or
  hand-rolled match?** Hand-rolled keeps the dep surface tight; revisit
  only if the tier count grows past four.

---

## 9. Verification plan

The swap is gated on **all** of:

1. **Compile-time:** `cargo check -p lunaris-extract` succeeds under the
   feature matrix:
   - `--no-default-features`
   - `--no-default-features --features extract-tiny`
   - `--no-default-features --features extract-small` (= today's
     `default = ["candle"]` shape minus the 4B impl)
   - `--no-default-features --features extract-medium`
   - `--no-default-features --features extract-large`
   - `--no-default-features --features extract-small,extract-large`
   - Mutually exclusive pair `--features extract-tiny,extract-medium`
     fails with the expected `compile_error!`.

2. **Trait conformance:** the existing `extractor_is_dyn_compat` test in
   `lunaris-extract/src/lib.rs` stays green for every new in-tree impl
   (so the `with_extractor(Arc<dyn Extractor>)` escape hatch remains
   valid).

3. **ER-F1 gates** *(Phase 24 bench harness)* per the table in §4. The
   `Small` tier MUST hit ≥ 0.70 on LongMemEval-S; otherwise the default
   remains `Medium` and this RFC partially ships (enum + builder only).

4. **Latency gates** *(criterion `--save-baseline v0.2.x`)*: ingest p50
   for one 1.5 k-token chunk on a reference 16 GB Mac:
   - `Tiny` ≤ 150 ms (CPU) / ≤ 60 ms (Metal)
   - `Small` ≤ 300 ms (CPU) / ≤ 100 ms (Metal)
   - `Medium` ≤ 700 ms (CPU) / ≤ 220 ms (Metal)
   - `Large` ≤ 1000 ms (95th percentile, network-inclusive)

5. **Memory floor gate** (the laptop story exit gate): the v0.2
   quickstart end-to-end (ingest 100 episodes + one recall) under the
   default `extract-small` feature uses ≤ 2 GB RAM measured via
   `/usr/bin/time -l` on macOS or `time -v` on Linux. Documented in
   `examples/quickstart-rs/README.md`.

6. **Cross-tier consistency:** ingesting the same episode under `Small`
   vs `Medium` produces validator outputs whose entity-ID sets overlap
   ≥ 80% (D-06 entity-ID is deterministic on canonical-name +
   entity-type, so this is a check on extraction recall, not on hashing).

7. **Inspection accessor test:** `Lunaris::builder().build().await?
   .extractor_tier()` returns `Some(ExtractorTier::Small)` under default
   features; `Lunaris::builder().extractor(custom).build()…
   .extractor_tier()` returns `None`.

8. **Deprecation-warning test:** building with `LUNARIS_EXTRACTOR_TIER=
   medium` while the compile-time default is `Small` emits exactly one
   `tracing::warn!` per process.

All eight gates are enumerated in the Phase 21 acceptance plan; failure
on (3), (4), or (5) is a v0.2.x release blocker — the rest are
correctness gates that block merge to `main`.

---

## 10. Decision log

- **2026-05-11** — RFC opened. Phase 21 P1 from
  `tmp/lunaris-ship-to-product-v2.md` formalized into a typed-dispatch
  RFC; the Phase 21 P0 Verifier 27B → 270M swap proceeds in parallel
  under its own plan (no RFC required — it is a configuration default,
  not a public-type change).
- **2026-05-11** — `Large` retained in-tree behind `extract-large`;
  separate crate decision deferred to v0.3 (§8 open question).
- **2026-05-11** — Default `default = ["candle", "extract-small"]`
  decided over `default = ["candle"]` + runtime cfg resolution — the
  feature-flag form is explicit in `cargo tree`, inspectable in build
  audits, and consistent with how `extract-medium` / `extract-large`
  will read.
