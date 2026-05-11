# RFC 0006 §4 — verifier divergence capture

This file is the landing zone for the **27B → 270M default-flip
quality gate**. The threshold is `divergence_rate ≤ 0.05` on a
100-item `NeedsReviewItem` corpus. Pass → the laptop-floor verifier
becomes the umbrella default (`verify-small` flipped on in
`crates/lunaris/Cargo.toml`). Fail → the flip is deferred until the
divergence is investigated.

## How to capture

```bash
# 1. Pre-flight: weights present?
ls ~/.cache/lunaris/models/gemma-3-27b-it     # ~14 GiB
ls ~/.cache/lunaris/models/gemma-3-270m-it    # ~600 MB

# 2. If either is missing:
huggingface-cli download google/gemma-3-27b-it \
    --local-dir ~/.cache/lunaris/models/gemma-3-27b-it
huggingface-cli download google/gemma-3-270m-it \
    --local-dir ~/.cache/lunaris/models/gemma-3-270m-it

# 3. Capture the number:
cargo run --release \
    --bin verifier-divergence \
    -p lunaris-bench \
    --features verify-small,verify-large \
    > tmp/verifier-divergence-$(date +%Y-%m-%d).txt

# 4. Append the result table below (or paste tmp/*.txt verbatim).
```

The binary prints stdout in markdown-table-friendly key/value form,
and stderr carries the diverging cases when the verdict is FAIL.

## Per-rig captures

> The first row of each table will land when the operator runs the
> binary on the corresponding rig. Until then the rows below are
> placeholders.

### `laptop-arm64` (M2 Pro / 16 GB / NVMe / macOS)

| Date       | Harness commit | corpus | matches | diverged | divergence_rate | threshold | verdict |
|------------|----------------|--------|---------|----------|------------------|-----------|---------|
| _pending_  | _pending_      | 100    | _pending_ | _pending_ | _pending_      | 0.05      | _pending_ |

### `server-x86` (EPYC 7763 / 64 GB / NVMe / Ubuntu 22.04)

| Date       | Harness commit | corpus | matches | diverged | divergence_rate | threshold | verdict |
|------------|----------------|--------|---------|----------|------------------|-----------|---------|
| _pending_  | _pending_      | 100    | _pending_ | _pending_ | _pending_      | 0.05      | _pending_ |

## What "divergence" means

Two `VerifyDecision`s are equivalent iff:

1. Both `applies()` returns the same boolean (no false-arbitrate vs
   abstain mismatch), AND
2. When both apply, `winner_id == winner_id` AND `loser_id == loser_id`.
   The `reason` string is intentionally NOT compared.
3. When both abstain, they are equivalent regardless of `reason`.

`divergence_rate = diverged / total`. The 0.05 threshold is RFC 0006
§4 — empirically reasonable for a 270M model carrying 99% of the
27B's arbitration signal.

## What happens on PASS

1. PR flips `verify-small` to default in `crates/lunaris/Cargo.toml`:

   ```toml
   default = ["verify-small"]    # was: default = []
   ```

2. Append the dated row above so the audit trail is permanent.
3. Update `docs/RELEASE.md` §3 noting the v0.2.x cut where the flip
   shipped.
4. Bump CHANGELOG.

## What happens on FAIL

1. Do NOT flip. The 27B stays default.
2. Inspect the stderr output (`Diverging cases (N of 100)`) — the
   first 5–10 cases usually show a pattern (e.g., 270M defers where
   27B arbitrates on transient-after-retry cases).
3. Three remediation paths, listed by escalating cost:
   - Tighten the prompt template in `candle_gemma3_270m.rs` to
     reduce the abstain rate.
   - Tighten the 270M `DEFAULT_PER_CHUNK_TIMEOUT_MS` if 270M is
     timing out and emitting `deferred`.
   - Defer the flip to v0.3 and ship `verify-small` as an opt-in
     feature only.
4. Re-run, capture the new number, repeat.
