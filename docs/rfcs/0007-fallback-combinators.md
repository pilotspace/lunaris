# RFC 0007 — `FallbackExtractor<P, F>` and `FallbackEmbedder<P, F>` with per-provider circuit breakers

| Field        | Value                                          |
|--------------|------------------------------------------------|
| Status       | **Draft** (2026-05-11)                         |
| Author       | Tin Dang                                       |
| Target       | Lunaris **v0.2.x** OSS                         |
| Supersedes   | —                                              |
| Related      | `tmp/lunaris-ship-to-product-v2.md` §3 (Phase 21) + §11, `tmp/xmem-grounded-findings-and-pickups.md` §2.3, RFC 0001 |

---

## 1. Summary

Introduce two generic, statically-dispatched combinators in the `lunaris-extract`
and `lunaris-embed` crates:

```rust
pub struct FallbackExtractor<P: Extractor, F: Extractor> { /* … */ }
pub struct FallbackEmbedder<P: Embedder, F: Embedder>     { /* … */ }
```

Each wraps a **primary** provider and a **fallback** provider. Both compose
recursively (`Fallback<A, Fallback<B, Fallback<C, Noop>>>`) so an N-deep
provider stack stays fully monomorphised — no `Box<dyn>`, no vtable on the hot
ingest path. A **per-provider circuit breaker** (Closed → Open → HalfOpen state
machine) trips after N transient failures in a sliding window, short-circuits
to the next provider, and self-heals on a HalfOpen probe success.

This is the typed-Rust port of XMem's `fallback_order: List[str]`
(`tmp/xmem-grounded-findings-and-pickups.md` §2.3, `settings.py:94`). XMem
resolves provider failover at runtime via string names; Lunaris resolves it at
compile time via generics. The functional surface is identical; the safety
surface — wrong-type fallbacks become a compile error — is strictly tighter.

The change is **purely additive**. The existing `Extractor` / `Embedder` traits
stay byte-for-byte; the combinators are new types that implement those traits.

---

## 2. Motivation

### 2.1 Today (v0.2.0)

Verified against `crates/lunaris-extract/src/lib.rs:117-136` and
`crates/lunaris-embed/src/lib.rs:38-40`:

- `Extractor` and `Embedder` are async traits with a single backend chosen at
  `Lunaris` construction time (`Lunaris::with_extractor` /
  `Lunaris::with_embedder`).
- `CloudApiExtractor` has an inline single-retry policy
  (`crates/lunaris-extract/src/cloud_api.rs:290-310`) and a sentinel
  `NeedsReviewReason::TransientAfterRetry` (D-21), but that policy is
  **per-call inside one provider** — it cannot fail over to a *different*
  provider.
- If Anthropic returns 503 for 10 minutes, the ingest path either stalls or
  marks every chunk `TransientAfterRetry`. There is no second provider to try.
- The Ollama backend (`OllamaEmbedder`, `OllamaExtractor`) has a 10s HTTP
  timeout but no concept of "this provider is sick, route around it."

### 2.2 Why now

1. **Phase 21 P0/P1 commits already increase provider diversity.** Verifier
   270M, Extractor tiers (`Tiny` 270M / `Small` 1B / `Medium` 4B / `Large`
   cloud) make a multi-provider stack the *default* shape (`tmp/lunaris-ship-to-product-v2.md` §3 Phase 21). Without a fallback combinator each
   tier swap is a hard cutover.
2. **The "Multi-LLM fallback" row in §11** of the ship plan calls
   `FallbackExtractor` out as the *mitigation* for "LLM provider deprecates a
   model mid-customer." Right now that row is uncovered.
3. **XMem's customers already expect it.** XMem ships `gemini → claude →
   openai → bedrock` as a documented feature; "Lunaris loses the customer to
   the first 30-minute Anthropic outage" is a real risk for the v0.2.x OSS
   window.
4. **Doing it generically is cheap; doing it dynamically is expensive.** The
   v0.1 → v0.2 review (RFC 0001 §10.x) confirmed that the hot ingest path
   monomorphises `Embedder` through `Lunaris<E>` and stays branch-predictable;
   a `Box<dyn Embedder>` regression on the per-chunk embed loop would cost
   measurable p99 (we have not budgeted for it).

### 2.3 What we are NOT doing in this RFC

- **No runtime provider registry** — providers are bound at type-construction
  time. XMem's `Vec<String>` shape is explicitly rejected (§5).
- **No retry policy unification** — `CloudApiExtractor`'s D-21 single-retry
  stays as-is. The breaker sits *above* per-provider retries; one provider's
  exhaustion is what trips the breaker for that provider.
- **No cross-instance breaker state** — the recommended behaviour is shared
  `Arc<CircuitState>` per provider key (§7), but the RFC does not mandate a
  global registry. Out-of-scope.
- **No fallback for `Verifier`** — Verifier already has a tier swap path
  (`tmp/lunaris-ship-to-product-v2.md` §3, Phase 21 P0) and runs on a worker,
  not the ingest critical path. Future RFC if needed.

---

## 3. Design

### 3.1 The combinator (extractor side)

```rust
// crates/lunaris-extract/src/fallback.rs
use async_trait::async_trait;
use std::sync::Arc;
use ulid::Ulid;

use crate::{ChunkInput, Extractor, RawExtractionBatch};
use lunaris_core::LunarisError;

/// A two-arm static-dispatch fallback. Stacks recursively:
///
/// ```ignore
/// FallbackExtractor::new(primary, FallbackExtractor::new(secondary, tertiary))
/// ```
///
/// The breaker is **per-instance, per-provider**. `provider_id` is the key
/// the breaker exposes via metrics and (optionally — §7 open question)
/// shares across instances pointing at the same upstream.
pub struct FallbackExtractor<P, F>
where
    P: Extractor,
    F: Extractor,
{
    primary: P,
    fallback: F,
    breaker: Arc<CircuitBreaker>,
    provider_id: ProviderId,
}

impl<P, F> FallbackExtractor<P, F>
where
    P: Extractor,
    F: Extractor,
{
    pub fn new(primary: P, fallback: F, provider_id: ProviderId) -> Self {
        Self {
            primary,
            fallback,
            breaker: Arc::new(CircuitBreaker::default_for(&provider_id)),
            provider_id,
        }
    }

    pub fn with_breaker(mut self, breaker: Arc<CircuitBreaker>) -> Self {
        self.breaker = breaker;
        self
    }
}

#[async_trait]
impl<P, F> Extractor for FallbackExtractor<P, F>
where
    P: Extractor,
    F: Extractor,
{
    async fn extract(
        &self,
        episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        if self.breaker.allow_request() {
            match self.primary.extract(episode_id, chunks).await {
                Ok(batch) => {
                    self.breaker.on_success();
                    metrics::counter!("fallback.primary.success",
                        "provider" => self.provider_id.as_str().to_owned()).increment(1);
                    return Ok(batch);
                }
                Err(e) if is_transient(&e) => {
                    self.breaker.on_failure();
                    metrics::counter!("fallback.primary.transient_failure",
                        "provider" => self.provider_id.as_str().to_owned()).increment(1);
                    // fall through to fallback
                }
                Err(e) => {
                    // Terminal — do NOT trip breaker; do NOT mask via fallback.
                    metrics::counter!("fallback.primary.terminal_failure",
                        "provider" => self.provider_id.as_str().to_owned()).increment(1);
                    return Err(e);
                }
            }
        } else {
            metrics::counter!("fallback.primary.tripped",
                "provider" => self.provider_id.as_str().to_owned()).increment(1);
        }

        metrics::counter!("fallback.secondary.attempt",
            "provider" => self.provider_id.as_str().to_owned()).increment(1);
        self.fallback.extract(episode_id, chunks).await
    }

    fn applies(&self) -> bool {
        self.primary.applies() || self.fallback.applies()
    }
}
```

The same shape applies symmetrically to `FallbackEmbedder<P, F>`:

```rust
// crates/lunaris-embed/src/fallback.rs
pub struct FallbackEmbedder<P, F>
where
    P: Embedder,
    F: Embedder,
{
    primary: P,
    fallback: F,
    breaker: Arc<CircuitBreaker>,
    provider_id: ProviderId,
}

#[async_trait]
impl<P, F> Embedder for FallbackEmbedder<P, F>
where
    P: Embedder,
    F: Embedder,
{
    fn dim(&self) -> usize {
        // Invariant: both arms MUST agree on dimension.
        // Enforced at `FallbackOrder::build` (debug_assert + Result on release).
        debug_assert_eq!(self.primary.dim(), self.fallback.dim());
        self.primary.dim()
    }

    async fn embed(&self, batch: &[String]) -> Result<Vec<Vec<f32>>, LunarisError> {
        // …same Closed/Open/HalfOpen flow as FallbackExtractor::extract
    }
}
```

**Why recursive instead of `[P; N]`:** an array of `P` forces a single concrete
backend type for every slot — that's the *opposite* of the goal. Recursive
nesting (`Fallback<Candle, Fallback<Ollama, Cloud>>`) lets each slot have a
different concrete type while keeping the whole stack monomorphised. The
ergonomic builder in §4 hides the nesting from callers.

### 3.2 `CircuitBreaker` state machine

```rust
// crates/lunaris-core/src/circuit.rs  (shared between extract + embed)
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct CircuitBreaker {
    state: Mutex<BreakerState>,
    config: BreakerConfig,
    /// Strictly increasing counter for HalfOpen probe arbitration —
    /// only one in-flight probe at a time.
    probe_token: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
pub struct BreakerConfig {
    /// Failures within `window` that flip Closed → Open.
    pub failure_threshold: u32,
    /// Sliding window for failure_threshold.
    pub window: Duration,
    /// Cooldown before Open → HalfOpen.
    pub cooldown: Duration,
    /// Successes on HalfOpen probes that flip HalfOpen → Closed.
    pub probe_successes_to_close: u32,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            window: Duration::from_secs(60),
            cooldown: Duration::from_secs(30),
            probe_successes_to_close: 2,
        }
    }
}

#[derive(Debug)]
enum BreakerState {
    Closed { failures: Vec<Instant> },
    Open   { opened_at: Instant },
    HalfOpen { successes: u32, in_flight_probe: bool },
}

impl CircuitBreaker {
    /// Called before issuing a request to the primary provider.
    /// Returns `true` if the request is allowed (Closed or HalfOpen probe slot);
    /// `false` if the breaker is Open and the cooldown hasn't elapsed.
    pub fn allow_request(&self) -> bool {
        let mut g = self.state.lock();
        match &mut *g {
            BreakerState::Closed { .. } => true,
            BreakerState::Open { opened_at } => {
                if opened_at.elapsed() >= self.config.cooldown {
                    *g = BreakerState::HalfOpen { successes: 0, in_flight_probe: true };
                    true   // this caller gets the probe slot
                } else {
                    false
                }
            }
            BreakerState::HalfOpen { in_flight_probe, .. } => {
                if !*in_flight_probe {
                    *in_flight_probe = true;
                    true
                } else {
                    false  // a probe is already in flight; route around
                }
            }
        }
    }

    pub fn on_success(&self) {
        let mut g = self.state.lock();
        match &mut *g {
            BreakerState::Closed { failures } => failures.clear(),
            BreakerState::HalfOpen { successes, in_flight_probe } => {
                *successes += 1;
                *in_flight_probe = false;
                if *successes >= self.config.probe_successes_to_close {
                    *g = BreakerState::Closed { failures: Vec::new() };
                }
            }
            BreakerState::Open { .. } => { /* unreachable — allow_request returned false */ }
        }
    }

    pub fn on_failure(&self) {
        let mut g = self.state.lock();
        match &mut *g {
            BreakerState::Closed { failures } => {
                let now = Instant::now();
                failures.retain(|t| now.duration_since(*t) <= self.config.window);
                failures.push(now);
                if failures.len() as u32 >= self.config.failure_threshold {
                    *g = BreakerState::Open { opened_at: now };
                }
            }
            BreakerState::HalfOpen { .. } => {
                *g = BreakerState::Open { opened_at: Instant::now() };
            }
            BreakerState::Open { .. } => { /* idempotent */ }
        }
    }
}
```

Notes:

- The lock is held **only across pure CPU work** — no `.await` while held
  (CLAUDE.md lock discipline, mirrored from Moon's UNSAFE_POLICY).
- `parking_lot::Mutex` (RFC 0001 convention).
- The HalfOpen `in_flight_probe` bit serialises probes so we don't slam a
  recovering provider with parallel traffic.

### 3.3 Error policy — transient vs terminal

The breaker MUST distinguish errors that indicate **provider sickness**
(retryable on the *same* provider, but worth failing over after enough of them)
from errors that indicate **caller fault** (the next provider will fail the
same way).

```rust
/// Returns `true` iff `e` is an upstream-transient condition (provider sick,
/// not caller-fault). Only transient errors trip the breaker.
fn is_transient(e: &LunarisError) -> bool {
    use lunaris_core::StorageError;
    match e {
        LunarisError::Storage(StorageError::Backend(msg)) => {
            // Mirror cloud_api.rs::is_transient classification:
            // 5xx, 429, timeout, connection-reset → transient
            // 4xx (except 429), grammar parse failure → terminal
            classify_backend_msg(msg) == ErrorClass::Transient
        }
        LunarisError::Timeout(_) => true,
        _ => false,
    }
}
```

Mapping rules (mirroring existing `cloud_api.rs` taxonomy where one exists):

| Error                                            | Class     | Trips breaker? |
|--------------------------------------------------|-----------|----------------|
| HTTP 5xx                                         | Transient | yes            |
| HTTP 429 (rate limit)                            | Transient | yes            |
| `tokio::time::error::Elapsed` / explicit timeout | Transient | yes            |
| Connection reset / DNS failure                   | Transient | yes            |
| HTTP 4xx (auth, schema)                          | Terminal  | no             |
| Grammar / JSON-schema parse failure              | Terminal  | no             |
| `NeedsReviewReason::GbnfFailure` sentinel        | Terminal  | no             |
| `LunarisError::Storage(WriteOpInvalid)`          | Terminal  | no             |

Terminal errors **propagate directly** — they are not masked by the fallback
arm, because the fallback would fail the same way and we'd return a less
useful error.

### 3.4 Observability

Every code path emits a `metrics::counter!` with `provider` and `outcome`
labels:

```text
fallback.primary.success
fallback.primary.transient_failure
fallback.primary.terminal_failure
fallback.primary.tripped              ← breaker Open, primary skipped
fallback.secondary.attempt
fallback.secondary.success
fallback.secondary.failure
fallback.breaker.opened               ← state transition events
fallback.breaker.half_opened
fallback.breaker.closed
```

Plus a gauge for current state per provider:

```text
fallback.breaker.state{provider="anthropic"}  = 0 (Closed) | 1 (Open) | 2 (HalfOpen)
```

These feed the v0.2 Grafana dashboard without further work — `metrics` crate
is already a transitive dep via the HTTP server.

---

## 4. Configuration shape — `FallbackOrder` builder

Writing the recursive type by hand is painful and locks the type signature.
Hide it with a fluent builder that returns an opaque
`impl Extractor` / `impl Embedder`:

```rust
// crates/lunaris-extract/src/fallback.rs
pub struct FallbackOrder<E: Extractor> {
    head: E,
    provider_id: ProviderId,
}

impl<E: Extractor> FallbackOrder<E> {
    pub fn new(primary: E, provider_id: ProviderId) -> Self {
        Self { head: primary, provider_id }
    }

    /// Push another provider behind `self`. Returns a new `FallbackOrder` whose
    /// `Extractor::extract` tries the existing chain first, then `next`.
    pub fn with_fallback<F: Extractor>(
        self,
        next: F,
        next_id: ProviderId,
    ) -> FallbackOrder<FallbackExtractor<E, F>> {
        FallbackOrder {
            head: FallbackExtractor::new(self.head, next, self.provider_id),
            provider_id: next_id,
        }
    }

    /// Build the final stack. Return type is `impl Extractor` so the caller
    /// never has to spell the nested generic.
    pub fn build(self) -> impl Extractor {
        self.head
    }
}
```

Call-site:

```rust
use lunaris_extract::{FallbackOrder, CandleGemma3_4B, OllamaExtractor, CloudApiExtractor, ProviderId};

let extractor = FallbackOrder::new(primary_candle, ProviderId::new("candle-4b"))
    .with_fallback(secondary_ollama,  ProviderId::new("ollama"))
    .with_fallback(tertiary_cloud,    ProviderId::new("anthropic"))
    .build();

let engine = Lunaris::open(moon_url).await?.with_extractor(Arc::new(extractor));
```

The `Arc<dyn Extractor>` form at the umbrella-handle boundary is unchanged —
the type-erasure happens at the `Lunaris::with_extractor` boundary, not on the
hot extract loop.

Symmetric `FallbackOrder` exists for `Embedder` in `lunaris-embed`. The
`Embedder::dim()` invariant (all arms must agree on output dimension) is
enforced in `build()`:

```rust
impl<E: Embedder> FallbackOrder<E> {
    pub fn build(self) -> Result<impl Embedder, FallbackBuildError> {
        // Walk the chain at construction time, assert dim() agreement.
    }
}
```

---

## 5. Rejected alternatives

| Alternative                                    | Rejected because                                                                                              |
|------------------------------------------------|----------------------------------------------------------------------------------------------------------------|
| `Vec<Box<dyn Extractor>>`                      | Kills static dispatch on the inner ingest loop — re-introduces vtable lookups on every chunk, on a path RFC 0001 §10.x explicitly stabilised. |
| Runtime config-only (XMem `fallback_order: List[str]`) | Loses compile-time wiring; a typo in TOML (`anthropc`) becomes a runtime 500 instead of a type error. |
| Trait-object behind a feature flag (`#[cfg(feature = "fallback-dyn")]`) | Half-fix — every caller who *might* enable the feature pays the indirection cost, and the type system can't tell. |
| Bake fallback into each backend (e.g., `CloudApiExtractor` learns about Ollama) | O(N²) coupling between backends; the fallback policy is orthogonal to any one backend. |
| `tower::Layer`-style middleware stack         | Right idea, wrong API for traits that aren't `Service<Request>`. Would force `Extractor` to become a `tower::Service`, a much larger change.  |
| Per-call retry only (no breaker)              | A 30-minute Anthropic outage produces 30 minutes of stalled retries instead of 30 ms of "Open, route around it." Doesn't solve the motivation. |

---

## 6. Compatibility

**Purely additive.**

- No change to `Extractor` or `Embedder` trait signatures (verified against
  `crates/lunaris-extract/src/lib.rs:117-136` and
  `crates/lunaris-embed/src/lib.rs:38-40`).
- No change to `Lunaris::with_extractor` / `Lunaris::with_embedder`.
- No change to `StoragePort`, `KeywordPort`, recipes, or the SDK shape.
- New public surface lives behind:
  - `lunaris_extract::{FallbackExtractor, FallbackOrder, ProviderId}`
  - `lunaris_embed::{FallbackEmbedder, FallbackOrder}`
  - `lunaris_core::circuit::{CircuitBreaker, BreakerConfig}` (shared).
- v0.1 / v0.2 callers continue to construct a single backend; nothing forces
  them onto the combinator.

---

## 7. Open questions

1. **Shared breaker state across `FallbackExtractor` instances pointing at the
   same provider?**

   *Scenario:* a process has two `FallbackOrder` chains (e.g., one for the
   ingest hot path, one for a backfill worker). Both wrap a `CloudApiExtractor`
   pointing at Anthropic. Should an outage detected by chain A automatically
   trip chain B's breaker for that provider?

   **Recommendation:** **yes**, via an optional shared `Arc<CircuitState>`
   keyed by `ProviderId`. A process-wide `CircuitRegistry` (default off) maps
   `ProviderId -> Arc<CircuitBreaker>` and `FallbackOrder::with_shared_breakers(&registry)`
   opts in. Default per-instance because cross-instance state implies a
   coordination contract we don't want forced on every caller.

2. **Backoff policy on HalfOpen probe failures: linear, exponential, jittered?**

   **Recommendation:** **jittered exponential** — `cooldown_n = base *
   2^min(n, cap) * Uniform(0.5..1.5)` where `n` is consecutive HalfOpen
   failures. The jitter prevents synchronised reconnect storms when N
   downstream Lunaris instances all probe the same recovering provider. Base
   = 30s, cap = 5 min, matches AWS SDK defaults.

3. **Should the breaker emit a structured event on state transition?**

   Sub-question of #4 below. **Recommendation:** yes, via `tracing::warn!`
   for Open transitions and `tracing::info!` for HalfOpen / Closed, in
   addition to the metrics counters. Operators want a log line; metrics are
   for dashboards.

4. **Per-scope breaker isolation?**

   In a multi-tenant deployment (RFC 0001), should scope A's traffic tripping
   the breaker affect scope B's traffic to the same provider? **Tentatively
   no** — the breaker is per-provider, not per-(provider, scope). A sick
   upstream is sick for everyone. But there's an argument for per-scope
   isolation when scope A is a noisy abuser. Defer to a v0.3 RFC if needed.

5. **`FallbackVerifier`?**

   Verifier (`crates/lunaris-verify/src/lib.rs:101`) has the same trait shape.
   Out of scope for this RFC because Verifier runs on the worker queue, not
   the ingest critical path — its tier-swap policy (Phase 21 P0) already
   gives an escape. Revisit if customers ask.

---

## 8. Verification plan

### 8.1 Unit tests — state machine

`crates/lunaris-core/src/circuit.rs`:

- `closed_stays_closed_below_threshold` — 4 failures with threshold=5, state
  stays Closed.
- `closed_to_open_on_threshold` — 5 failures in window, state flips Open.
- `open_rejects_until_cooldown` — `allow_request` returns false until
  `cooldown` elapses.
- `open_to_half_open_on_cooldown_expiry` — after cooldown,
  `allow_request` returns true and state flips HalfOpen.
- `half_open_to_closed_on_probe_successes` — N successes flip back to Closed.
- `half_open_to_open_on_probe_failure` — single failure flips back to Open
  with a fresh `opened_at`.
- `half_open_serialises_probes` — second concurrent `allow_request` returns
  false while a probe is in-flight.
- `sliding_window_evicts_old_failures` — failures older than `window` drop
  out of the count.

### 8.2 Integration test — mock-flaky extractor

`crates/lunaris-extract/tests/fallback_combinator.rs`:

```rust
struct FlakyExtractor {
    plan: Mutex<VecDeque<Outcome>>,  // deterministic failure injection
    id: &'static str,
}

#[async_trait]
impl Extractor for FlakyExtractor {
    async fn extract(&self, _eid: Ulid, _chunks: &[ChunkInput])
        -> Result<RawExtractionBatch, LunarisError> { /* pop from plan */ }
}
```

Scenarios:

1. **Primary healthy** — all primary calls succeed, fallback never invoked,
   breaker stays Closed.
2. **Primary trips, fallback serves** — primary returns 5 transient errors,
   breaker opens, next 10 calls route to fallback without touching primary.
3. **Primary recovers** — after cooldown, one HalfOpen probe succeeds, next
   probe succeeds, breaker closes, primary serves again.
4. **Terminal error bypasses fallback** — primary returns a terminal error,
   we get that error back directly; breaker stays Closed.
5. **Both arms fail transient** — error from the *fallback* propagates (last
   arm in the chain has no further fallback).
6. **3-deep chain** — primary fails terminal-no, secondary opens-breaker,
   tertiary serves.

### 8.3 Loom test — breaker concurrency

`crates/lunaris-core/tests/circuit_loom.rs` (gated `cfg(loom)`):

- Two threads concurrently call `allow_request` / `on_failure` / `on_success`
  against a single `CircuitBreaker`. Loom explores all interleavings.
- Asserts: no state transition violates the diagram (e.g., never Open →
  Closed without HalfOpen), no two concurrent probes succeed past the
  `in_flight_probe` gate.

### 8.4 Bench — hot-path regression budget

`crates/lunaris-embed/benches/fallback_embed.rs` (criterion):

- Baseline: `CandleEmbeddingGemma` alone, 1k embed calls.
- With combinator: `FallbackEmbedder<Candle, Ollama>` (Ollama unreachable,
  but breaker Closed so it never gets called on the happy path).
- **Budget:** ≤ 2% p50 regression vs baseline. Anything above and we
  re-examine the lock granularity (likely move to an `ArcSwap` of an
  immutable state snapshot on the happy path).

### 8.5 Workspace verifier gates

The standard RFC 0001 gates apply:

- `cargo build --workspace --all-features` ✅
- `cargo clippy --workspace -- -D warnings` ✅
- `cargo test --workspace` ✅ (excluding `lunaris-py` / `lunaris-ts` cdylibs)
- `cargo fmt --check` ✅
- INGEST-04 invariant unchanged (single `atomic_write` per ingest path).
- New: `grep -c 'Box<dyn Extractor>' crates/lunaris-extract/src/fallback.rs`
  must return **0** — the whole point of this RFC.

---

## 9. Decision log

- **2026-05-11** — Draft opened. Picks up XMem §2.3 with typed-Rust framing.
  Generic-recursive shape chosen over `Vec<Box<dyn>>` to preserve hot-path
  static dispatch. RFC ID 0007 (RFCs 0002–0006 reserved in the ship plan
  appendix, see `tmp/lunaris-ship-to-product-v2.md:322-325`).

---

## 10. Acceptance criteria

Status flips to **Implemented** when:

- `FallbackExtractor<P, F>` and `FallbackEmbedder<P, F>` ship in their
  respective crates, gated `feature = "fallback"` (default-on — additive cost
  is zero when unused).
- `FallbackOrder` builder is exported from both crates.
- `CircuitBreaker` + `BreakerConfig` ship in `lunaris-core::circuit`.
- All §8.1–§8.4 tests are green; §8.4 regression is within budget.
- `tmp/lunaris-ship-to-product-v2.md` §11 "Multi-LLM fallback" row is marked
  covered.
- `docs/recipes/fallback.md` (new) shows a worked end-to-end example with
  three providers + a deliberately-killed primary.
