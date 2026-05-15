# Lunaris v0.4.x — per-device perf contracts

> **Any number outside these envelopes blocks the release.**
>
> This is the falsifiable form of the hardware-target gate table from
> `.planning/milestones/v0.4-NATIVE-SINGLE-BACKEND/HARDWARE-OPTIMIZATION-ROADMAP.md`.
> CI enforces it via `.github/workflows/perf-gates.yml` (relative drift,
> 5% cliff) + the absolute checks in §3 below (operator-asserted at
> baseline-bless time via `.github/workflows/perf-baseline-save.yml`).

## 1. Two gate types — relative vs absolute

| Gate type | What it asserts | Where it's enforced | Failure → |
|-----------|-----------------|----------------------|-----------|
| **Relative drift** | "No bench regressed > 5% vs the saved baseline" | `perf-gates.yml` → `perf-gate-check` binary (O-03-A) | Block PR merge |
| **Absolute hardware-target** | "Apple Silicon Metal embed p50 ≤ 5 ms, etc." | Operator asserts BEFORE `perf-baseline-save.yml` is invoked | Block baseline-bless |

The relative cliff is what CI gates on every PR. The absolute envelope
is what the operator confirms before promoting a new baseline — if a
new bench number meets the relative gate but blows past the absolute
ceiling, the baseline-save is rejected and a release-blocking issue
filed.

## 2. Absolute gate table (HARDWARE-OPTIMIZATION-ROADMAP §26-37)

Measured at **batch=8** for embed and **K=10** for rerank, FP16 weights
(GGUF Q4/Q5 variants tracked separately under `embedder-gguf` /
`reranker-gguf` features).

| Host                          | Embed p50 | Embed p99 | Rerank p50 (K=10) |
|-------------------------------|-----------|-----------|--------------------|
| Apple Silicon (Metal)         | ≤ 5 ms    | ≤ 15 ms   | ≤ 40 ms            |
| NVIDIA sm_80+ (CUDA)          | ≤ 3 ms    | ≤ 10 ms   | ≤ 25 ms            |
| x86_64 CPU + Accelerate/MKL   | ≤ 40 ms   | ≤ 90 ms   | ≤ 300 ms           |
| aarch64 CPU + NEON            | ≤ 80 ms   | ≤ 200 ms  | ≤ 600 ms           |

## 3. Current measurements

Numbers below are the **blessed baselines** that `perf-gate-check`
compares against. TBD entries mean the self-hosted runner is not yet
online; the operator will fill them in via the procedure in §6 as
runners come up.

### apple-metal (self-hosted, macos, arm64, metal)

| Metric            | Gate     | Measured | Margin | Last refresh |
|-------------------|----------|----------|--------|--------------|
| embed/batch_8 p50 | ≤ 5 ms   | TBD      | TBD    | —            |
| embed/batch_8 p99 | ≤ 15 ms  | TBD      | TBD    | —            |
| rerank/k10 p50    | ≤ 40 ms  | TBD      | TBD    | —            |

### cuda-sm80 (self-hosted, linux, x86_64, cuda)

| Metric            | Gate     | Measured | Margin | Last refresh |
|-------------------|----------|----------|--------|--------------|
| embed/batch_8 p50 | ≤ 3 ms   | TBD      | TBD    | —            |
| embed/batch_8 p99 | ≤ 10 ms  | TBD      | TBD    | —            |
| rerank/k10 p50    | ≤ 25 ms  | TBD      | TBD    | —            |

### cpu-x86_64-mkl (ubuntu-latest, GH-managed)

| Metric            | Gate     | Measured | Margin | Last refresh |
|-------------------|----------|----------|--------|--------------|
| embed/batch_8 p50 | ≤ 40 ms  | TBD      | TBD    | —            |
| embed/batch_8 p99 | ≤ 90 ms  | TBD      | TBD    | —            |
| rerank/k10 p50    | ≤ 300 ms | TBD      | TBD    | —            |

### cpu-aarch64-neon (ubuntu-22.04-arm, GH-managed)

| Metric            | Gate     | Measured | Margin | Last refresh |
|-------------------|----------|----------|--------|--------------|
| embed/batch_8 p50 | ≤ 80 ms  | TBD      | TBD    | —            |
| embed/batch_8 p99 | ≤ 200 ms | TBD      | TBD    | —            |
| rerank/k10 p50    | ≤ 600 ms | TBD      | TBD    | —            |

## 4. Regression cliff — 5%

`perf-gate-check` (binary at `crates/lunaris-bench/src/bin/perf_gate_check.rs`)
reads Criterion's `target/criterion/<group>/<bench>/change/estimates.json`
after a `cargo bench -- --baseline v0.4-O01-<label>` run. The
load-bearing field is `median.point_estimate` — relative change vs the
saved baseline (0.05 = +5%).

A bench fails the gate iff `median.point_estimate > 0.05`. Improvement
(negative delta) is always green. The mean is **not** gated on —
median is more robust to the cold-cache spike on the first few
iterations.

p99 is **not** part of the relative cliff (Criterion's `estimates.json`
only stores mean/median/std/MAD/slope). The p99 contract lives in §2 and
is asserted at baseline-bless time by the operator reading
`target/criterion/.../sample.json` and computing the 99th percentile
manually. See §6.

## 5. How CI uses these numbers

```
PR opened (label `perf-bench` applied)
    │
    ▼
.github/workflows/perf-gates.yml fires (matrix × 4 hosts)
    │
    ├─ download-artifact perf-baseline-v0.4-O01-<label>
    ├─ cargo bench --features <device> --bench per_device -- --baseline v0.4-O01-<label>
    ├─ cargo run --bin perf-gate-check -- --criterion-dir target/criterion --threshold 0.05
    │       ├─ exit 0 → matrix cell green
    │       ├─ exit 1 → matrix cell red (regression)
    │       └─ exit 2 → matrix cell red (operator error)
    ▼
perf-gates-summary collapses 4 cells → single status
```

The `perf-gates-summary` status is what the operator sets as
branch-protection required once all four runners are healthy. The flip
procedure lives in `docs/runbooks/v0.4-self-hosted-runners.md` §6.

## 6. How to refresh the baseline

Use this procedure when an intentional perf change lands (new model
weights, candle bump, kernel rewrite). **Do not** refresh to "make CI
green" — that's how a regression becomes the new floor.

### 6.1 Pre-flight (the operator's responsibility)

1. Run `cargo bench --features <device> --bench per_device --
   --save-baseline scratch-<your-name>` on the target host (locally or
   on the self-hosted runner via SSH).
2. Read the p50/p99 numbers from the Criterion output:
   - **p50**: `target/criterion/embed/<backend>/batch_8/scratch-<name>/estimates.json` → `median.point_estimate` (nanoseconds → divide by 1e6 for ms).
   - **p99**: `target/criterion/embed/<backend>/batch_8/scratch-<name>/sample.json` → 99th percentile of the per-iteration timing list.
3. Compare against the absolute gate table (§2). If any number is over
   the ceiling, **STOP** — file a release-blocking issue, do NOT proceed.
4. If all numbers are under the ceiling, fill in the §3 table for the
   relevant matrix cell and commit the doc change in the same PR that
   landed the intentional perf change.

### 6.2 Promote to blessed baseline

1. Go to Actions → `perf-baseline-save` → `Run workflow`.
2. Fill in `reason` with a one-line description (commit SHA of the
   intentional perf change + short rationale).
3. Optionally restrict `targets` to a comma-separated subset if only
   one host needs refreshing.
4. The workflow's `Verify baseline files exist` step asserts the bench
   actually wrote output. If it fails, the most likely cause is missing
   `LUNARIS_STAGE_MODELS_KEY` — see the runbook.
5. The uploaded `perf-baseline-v0.4-O01-<label>` artifact is the new
   floor. `perf-gates.yml` will pick it up on the next PR run.

## 7. Reference hardware

The self-hosted runner specs MUST match these baselines or the absolute
gates become meaningless. See
`docs/runbooks/v0.4-self-hosted-runners.md` §1 for the canonical specs:

| Cell              | Min spec |
|-------------------|----------|
| `apple-metal`     | M2 Pro 10-core or better, 16 GB unified memory, macOS 14.x |
| `cuda-sm80`       | NVIDIA A10 / A100 / RTX 30/40-series (sm_80+), 24 GB VRAM, Ubuntu 22.04 |
| `cpu-x86_64-mkl`  | GH-managed `ubuntu-latest` (current: Intel Xeon Platinum 8370C, 2 vCPU) |
| `cpu-aarch64-neon`| GH-managed `ubuntu-22.04-arm` (current: Cobalt 100, 2 vCPU) |

If the operator upgrades a runner, the baseline MUST be re-blessed via
§6 — old numbers from weaker hardware would become a stricter cliff
than reality, blocking valid PRs.

## 8. Where this fits in the v0.4 release-blocker chain

| Phase | Deliverable | Status |
|-------|-------------|--------|
| N-04 D4 | `ci-perf-gates` feature flag in `lunaris-bench` | Shipped |
| O-01-F | `per_device.rs` Criterion harness + per-device features | Shipped |
| O-01 docs | `docs/benchmarks/v0.4-O01-baselines.md` measurement procedure | Shipped |
| **O-03-A** | **`perf-gate-check` binary + unit tests** | **Shipped (this milestone)** |
| **O-03-B** | **`perf-gates.yml` CI workflow** | **Shipped (this milestone)** |
| **O-03-C** | **`perf-baseline-save.yml` operator workflow** | **Shipped (this milestone)** |
| **O-03-D** | **This doc** | **Shipped (this milestone)** |
| **O-03-E** | **`docs/runbooks/v0.4-self-hosted-runners.md`** | **Shipped (this milestone)** |
| O-03 follow-up | Operator provisions 2× self-hosted runners + fills TBD numbers | **Not started** (out of scope for the executor; runbook §1-5 is the contract) |

After the operator completes the follow-up, the `perf-gates-summary`
check can be flipped to branch-protection-required. Until then it's
advisory.
