# Contributing to Lunaris

Thanks for taking the time. Lunaris is a production agent-memory engine, so
the bar for a change is "a stranger can see why it is correct", not "it
compiles".

## Ground rules

1. **Every claim is measured or gone.** If you add a number to a README, a
   doc, or a doc comment, it must trace to a committed artifact — a benchmark
   report with its raw samples under `docs/benchmarks/`, or
   [`docs/operations/capacity.md`](docs/operations/capacity.md). Numbers
   without a source get deleted, not softened.
2. **Red/green TDD.** Land a failing test that describes the behaviour, then
   the change that makes it pass. A red test must be *satisfiable* — walk the
   future fix through the harness before you accept the red.
3. **Built is not wired.** A new component needs a test proving the
   *production* path invokes it, not just that the component works in
   isolation.
4. **No new `unsafe`** without a `SAFETY:` comment stating the invariant.

## Prerequisites

| Requirement | Version |
|---|---|
| Rust | **1.94** (MSRV — matches Moon, so cross-repo work is painless) |
| Rust edition | 2024 |
| `cmake` + a C++ compiler | needed by the default `llamacpp` feature (llama.cpp) |
| Python | 3.11+ (only for `lunaris-py`) |
| Node | 20+ (only for `lunaris-ts`) |
| Moon | required for storage-backed tests — see below |

A `--no-default-features` build is the **Tier-0, no-inference** build and
needs no C++ toolchain. Use it if you only touch pure-Rust crates.

## Build and test

```sh
# Format — NEVER `cargo fmt --all`: --all also formats the path deps under
# vendor/, which are not ours. CI enumerates workspace members instead:
cargo metadata --no-deps --format-version=1 | jq -r '.packages[].name' \
  | sed 's/^/-p /' | xargs cargo fmt --check

# Lint — workspace-wide AND all targets. Per-crate clippy misses cross-crate
# exhaustive-match breaks.
cargo clippy --workspace --all-targets -- -D warnings

# Test. lunaris-py and lunaris-ts are cdylibs and cannot link in a plain
# `cargo test` run; they are excluded here and tested via maturin/napi.
cargo test --workspace --all-targets --exclude lunaris-py --exclude lunaris-ts
```

### Storage-backed tests need a Moon binary

The harness spawns its **own ephemeral Moon on a random port** — it never
touches a running instance. Point it at a binary:

```sh
export MOON_TEST_BINARY=/path/to/moon
```

Without it, storage-backed tests fail with *"could not start an ephemeral
Moon"*. Inside a linked git worktree the `vendor/moon` submodule build does
not work, so this variable is mandatory there.

**Never point tests at ports 6379 / 6380 / 6381.** Those are conventionally a
developer's live stores on this project's reference machines, and a stray
`FLUSHALL` is unrecoverable. Benchmark instances live on 6399+.

### SDK tests

```sh
# Python
maturin develop --release -m crates/lunaris-py/Cargo.toml && pytest crates/lunaris-py/tests

# TypeScript
cd crates/lunaris-ts && npm ci && npm run build && npm test
```

## What CI checks

These run on every PR. **Branch protection is not yet configured on `main`**
(it needs org admin), so treat them as the standard you are held to by review,
not as a gate that will physically stop a merge:

| Gate | What it asserts |
|---|---|
| `cargo fmt --check` | formatting, workspace members only |
| `cargo clippy --workspace --all-targets -D warnings` | zero warnings |
| `cargo test --workspace` | the suite, minus the two cdylib SDK crates |
| `cargo deny check` | advisories, licences, bans, sources |
| `cargo_semver_checks` | no accidental breaking change in a published crate |
| `cargo_publish_dry_run_core` | the publishable crates still package |
| `ingest_04_single_atomic_write` | exactly **one** `storage.atomic_write` call site in `crates/lunaris-ingest/src/pipeline.rs` |
| `lunaris_core_leaf_purity` | `lunaris-core` stays dependency-light |
| `version_parity_rust_python_typescript` | the three SDK versions agree |
| `parity-check` | regenerated bindings match the committed snapshots |
| `recall-ratchet` | judge-free LongMemEval-S any-gold hit-rate against a committed, config-signature-locked baseline |

**Not enforced in CI:** the recall *latency* contract. `perf-gates.yml` is
opt-in behind a `perf-bench` label and is not a required check. The latency
envelope is produced by a **manual, local** ~10-minute live-Moon run:

```sh
scripts/bench/perf/recall_latency.sh all
```

Do not describe any latency number as CI-gated.

## Invariants worth knowing before you start

These are grep-pinned; breaking one fails CI, and re-introducing one by hand
is the most common way a PR gets sent back.

- **INGEST-04 — one `atomic_write` per ingest.** A new ingest fan-out extends
  the single `WriteOp` vector; it never adds a second `atomic_write`.
- **Never hold a lock across `.await`.** Snapshot under `read()`/`write()`,
  drop the guard, then await. Use `parking_lot`, never `std::sync::*Lock`.
- **Keyspace helpers live in `lunaris-core`.** Minting a Lunaris KV key from a
  local helper is a bug — use `lunaris_core::keyspace::*`.
- **Every public request DTO carries `#[serde(deny_unknown_fields)]`.**
  Without it a client can smuggle a `scope` override past the token-bound
  partition key.
- **Every MCP `#[tool]` response schema root must be `type: "object"`.** A
  `#[serde(tag = …)]` enum yields a root `oneOf` and aborts server startup for
  *all* builds. Response DTOs are flat structs carrying a `status` field.
- **`embedded-moon` never appears in a default feature set.**
- **No `.rs` file exceeds 1500 lines**; split read/write paths at 1000.

## Commits and pull requests

Conventional-commit style:

```
<type>(<scope>): <short summary>

<body — what changed, why, and what evidence backs it>

<footer — refs, breaking changes>
author: Your Name <you@example.com>
```

Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `perf`, `ci`,
`build`, `style`, `revert`.

- Keep red/green pairs as **separate commits** — the red is the evidence.
- One logical change per PR. A PR that fixes a bug and reformats a file is two
  PRs.
- Fill in the PR template. The "how was this verified" box is the point of it.

## Reporting security issues

Do **not** open a public issue. See [`SECURITY.md`](SECURITY.md).

## Licence

By contributing you agree your work is licensed under
[Apache-2.0](LICENSE), the project's licence.
