# Lunaris v0.2.x — reproducible benchmarks

This directory is the public landing zone for **reproducible**
performance numbers on Lunaris v0.2.x. Every number that appears on
the project README, in a blog post, or in a docs.rs tagline must be
producible by running a single `make` target on the documented
reference hardware.

> Note: the v0.2.x release line ships the benchmark harness; the
> numbers themselves land here as they're captured. Run the targets
> below against your own infrastructure to validate — Lunaris's
> Core Value ("sub-25ms recall over millions of bi-temporal facts")
> is the contract being tested.

## TL;DR — how to run

```bash
# 1. Bring up Moon + Postgres (5-min docker-compose at examples/quickstart-rs/).
docker compose -f examples/quickstart-rs/docker-compose.yml up -d

# 2. Point the benches at them.
export MOON_URL=moon://localhost:6380
export PG_URL=postgres://lunaris:lunaris@localhost:5432/lunaris

# 3. Capture a baseline.
make bench-public

# 4. Numbers land in target/benches/v0.2.1/ as bencher-format CSV.
ls target/benches/v0.2.1/
#   recall.bencher.txt
#   ingest.bencher.txt
#   atomic_write.bencher.txt
```

## Reference hardware

We publish numbers measured on **two** rigs so the floor + ceiling
are both visible. Any internal/external comparison must cite which
rig the number came from.

| Rig            | CPU                       | RAM   | Disk         | OS              |
|----------------|---------------------------|-------|--------------|-----------------|
| `laptop-arm64` | Apple M2 Pro 10-core      | 16 GB | NVMe (built-in) | macOS 14.x      |
| `server-x86`   | AMD EPYC 7763 (16-core slice) | 64 GB | NVMe (local)    | Ubuntu 22.04 LTS|

Moon and Postgres run on the **same host** as the bench process —
not over a network. This is the "embedded substrate" deployment
shape that v0.2.x targets; networked-substrate numbers are a v0.3
target.

## Targets + what they measure

| `make` target           | Bench source                                          | Measures                                            |
|--------------------------|-------------------------------------------------------|-----------------------------------------------------|
| `make bench-recall`      | `crates/lunaris-bench/benches/recall_hot_path.rs`     | recall p50 / p99 over a pre-warmed 100k-fact corpus |
| `make bench-ingest`      | `crates/lunaris-bench/benches/ingest_hot_path.rs`     | end-to-end episode ingest p50 (chunk → embed → write)|
| `make bench-ingest`      | `crates/lunaris-bench/benches/atomic_write_hot_path.rs` | `atomic_write` per-op throughput (no embed in path)|
| `make bench-helios`      | `crates/lunaris-bench/benches/helios_p50.rs`          | end-to-end Helios recall flow (gated on Helios infra)|
| `make bench-baseline`    | All of the above                                      | runs everything, saves under `BASELINE=v0.2.1`      |

The `--save-baseline $(BASELINE)` flag is set to `v0.2.1` in the
top-level Makefile. Re-running against a future release line uses
`--baseline v0.2.1` to surface regressions in the criterion delta
column.

## What the contract is

The Core Value (CLAUDE.md §Core Value) commits to **p50 < 25 ms
recall over millions of bi-temporal facts** on the laptop-arm64 rig.
The v0.1 prior-art harness (`scripts/test-recovery.py`) already
clocked **p50 10.3 ms / p99 20.8 ms** on a strict-replay path against
a live Moon at 6380 (see auto-memory `reference_lunaris_benchmarks.md`).

The v0.2 contract:

1. **recall_hot_path** p50 ≤ 25 ms on `laptop-arm64`
2. **recall_hot_path** p50 ≤ 10 ms on `server-x86`
3. **recall_hot_path** p99 ≤ 100 ms on either rig
4. **ingest_hot_path** p50 ≤ 250 ms on `laptop-arm64` (chunk + embed
   dominate the path — this is a CPU-bound contract, not an I/O one)

Any number outside these envelopes blocks the release.

## Where the captured numbers go

When a release ships, the harness output gets committed to:

```
docs/benchmarks/v0.2.x/
├── README.md             (this file)
├── laptop-arm64.md       (numbers from rig 1)
├── server-x86.md         (numbers from rig 2)
└── raw/                  (raw bencher-format CSVs, audit trail)
    ├── v0.2.1/
    │   ├── recall.bencher.txt
    │   ├── ingest.bencher.txt
    │   └── atomic_write.bencher.txt
    └── v0.2.2/...
```

Each per-rig `.md` carries: harness commit SHA, rig spec, Moon /
Postgres versions, the actual p50/p99 numbers, criterion's
`change-over-baseline` column when applicable, and a date stamp.

## Why this matters for shipping

Crates.io listings get exactly one shot at the "is this fast?"
question. We answer it with a reproducible runbook, not a marketing
chart. If a downstream user runs `make bench-public` on their own
infra and gets numbers within 2x of ours, the contract holds. If
they get 10x worse, that's either an infra issue (worth filing) or
a regression (worth blocking on).

Phase 19 of `tmp/lunaris-ship-to-product-v2.md` calls for "baseline
numbers in a public, version-controlled location, reproducible by
anyone with a Cargo toolchain." This directory is that location.
