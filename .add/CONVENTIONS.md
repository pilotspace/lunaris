# CONVENTIONS  (living documentation — set once, kept for the whole project)

<!-- evidence-grounded: root CLAUDE.md, Cargo.toml [workspace], rustfmt.toml, CI workflows -->

Language/framework: Rust edition 2024, MSRV 1.94 · Python 3.11+ (PyO3 0.26) · TypeScript Node 20+ (napi-rs 3.x)
Folders: cargo workspace `crates/lunaris-*`; vendored substrate `vendor/moon` (submodule, BOTH path-dep `moondb` AND server binary); planning `.planning/` (GSD, own submodule); ADD task files `.add/tasks/<slug>/TASK.md` — but code+tests live in `crates/` per cargo convention, NOT in `.add/tasks/<slug>/src/`
Naming: snake_case files/functions, PascalCase types; crate names `lunaris-<context>`
Lint/format: `cargo fmt --all` + `cargo clippy --all-targets -- -D warnings` (zero-warnings, enforced in CI); fmt-check is branch-wide, clippy ≠ fmt
Errors: `thiserror 2.x` enums in library crates (e.g. `StorageError`), `anyhow` only in binaries; HTTP DTOs carry `#[serde(deny_unknown_fields)]`
Architecture: ports-and-adapters — `lunaris-core` owns ports (`StoragePort`, `KeywordPort`) + keyspace helpers; backend crates implement, never mint keys locally; engine crates depend on ports, not backends
Testing: red/green TDD; integration tests against Moon + Postgres + SQLite simultaneously (conformance suite); `cargo test --workspace` EXCLUDES lunaris-py/lunaris-ts (cdylib link errors — use maturin+pytest / napi+vitest); live-Moon tests env-gated
Locks: `parking_lot` only; NEVER hold a guard across `.await`
File size: no `.rs` file over 1500 lines; split read/write at 1000
Commits: `<type>(<scope>): summary` + body + `author: Tin Dang` footer; message written to `tmp/<name>.txt`, committed via `git commit -F`; atomic per task
Submodule discipline: never pin a vendor/moon commit not pushed to pilotspace/moon; `.planning` commits land in its own repo first, then gitlink bump

## Folded conventions — moon-v030-exploit retrospective (2026-06-11, foundation v2)

Testing (TDD):
- moon-it suite isolation: each integration-test binary gets its own server (fresh `--dir`) or its own ULID-scoped FT indices; never share one live Moon across suites that create global indices (dim_configurable has both cross-suite pollution AND a within-suite race).
- Graceful-skip hardening: `connect_or_skip` must distinguish "unreachable" (skip) from "reachable but incompatible" (fail); CI sets `MOON_IT_REQUIRED=1` to turn connect-skips into hard failures (false-pass struck twice).
- Default fixture style for storage-visible features: the production-path discriminator — seed ONLY through `atomic_write`, assert the feature observable end-to-end.
- Additive port evolution: "additive trait default method + `StorageCapabilities` flag" is the proven recipe (queue_depth, decay, navigate, hot_keys); the compiler-checked literal sweep over ~40 sites is acceptable cost.
- Sampling-based live tests engineer an exact traffic→expectation ratio (4096 pipelined GETs ÷ 64 sampling = exactly 64) instead of probabilistic asserts.
- Contracts below an SDK boundary name observable PROPERTIES (stream stays alive), not exact error strings — adapter layers absorb failure modes.
- Recall/quantization evals MUST state corpus realism: synthetic random vectors invert tier rankings (tq4 0.405 synthetic vs ~0.89 real).
- Corpus floors are validated against the SPLIT, not the dataset (SQuAD validation = 2,067 unique contexts < a 3k floor).

Build/harness (ADD):
- Probe-before-freeze: a one-shot live probe against the real server converts freeze ⚠ flags into ground facts before red tests exist (paid off 3×: ADDNODE prop coercion, GRAPH.SETPROP docs-drift, QUANTIZATION arity).
- Probe scripts launch their OWN servers in tempdirs — immune to the SO_REUSEPORT stale-listener trap; for doc-spikes the probe script IS the executable test (exit 0 = evidence matrix reproduces).
- Long-running benches: the log file with periodic progress markers is the single source of truth; `ps`-grep false-negatives on column truncation.

## Folded conventions — multi-milestone retrospective (2026-06-16, foundation v3)

Testing (TDD):
- Production-path seeding is mandatory for read-surface tests: seed via the real write path (`ingest_structured_inner`), never hand-seed convenient row shapes — browse shipped green but prod-broken from hand-seeded `core::Fact` (built ≠ wired).
- Seed at the REAL production key, not a convenient relative string — a mock keyed to the same buggy scope-less `format!("fact:{ulid}")` masked a broken `apply_supersede` through its unit test.
- Walk the future-green through the harness before accepting a red: a `take()`-consumed receiver or a "hang vs no-hang" deadline test can be red-for-the-wrong-reason / pass against UNFIXED code. For a timeout fix the discriminator is that the CONFIGURED bound CHANGES (fake server completes handshake then stalls; assert elapsed ∈ [2s,6s]).
- A mock that records a query STRING cannot catch a backend that mis-executes it — graph/Cypher contracts need a live-backend discriminating test on the production path (the inline-filter bug passed the mock gate, surfaced only in live UAT).
- Un-CI-able production path → split the discriminator: a BEHAVIORAL test of the EXTRACTED seam (the real production code the gated arm calls, not a test-only copy) + a STRUCTURAL `include_str!` source-guard on the call-site (≈3GB candle weights; full live-Moon connect dialog).
- The adversarial subagent refute-read is load-bearing, not ceremonial — caught a real MEDIUM bug (stale spo-index window → false supersede) that the test plan and the author's green both missed.
- Dependency-bump RED is the advisory-DB check itself (`cargo deny check advisories` red on the old pin), not a hand-written test.
- Under `#![forbid(unsafe_code)]` env config can't be unit-tested via `std::env` mutation (`set_var` is `unsafe`): split a pure parser the unit test drives + cover the env wrapper end-to-end.
- Timing assertions must name their regime — absolute vs ratio bounds discriminate in OPPOSITE speed regimes; pick per measured serialized estimate.
- Absence-of-API tests assert via `typeof`/`hasattr`, never an error-message regex (a TypeError message can satisfy a throw-pattern by accident).

Build/harness (ADD):
- Frozen test files must NOT be edited during build — even a clippy/doc/`cargo fmt` fix trips the scope-lock test-tamper flag; pre-empt lint/fmt when authoring; clean re-baseline is `add.py phase tests` → re-advance (suggests an fmt-aware scope-walk carve-out).
- Harness-machinery fixes during tests/build are legitimate ONLY when zero assertions change — commit separately and say so.
- Structural `include_str!` guards live in a `tests/` file (reading `../src/<file>.rs`), not in-module, when the target is over the size budget (handle.rs = 2452 lines) — avoids growth + self-match.
- A contract's "no change expected for dep X" PREDICTS, it doesn't CONSTRAIN: when false at build AND the fix is convention-mandated + in-scope, fix in-scope (discriminating red→green + §6 flag) instead of a change-request round-trip.
- A "BLOCKED-ON-UPSTREAM" contract needs a DATED recheck artifact — an unowned "weekly-ish probe" left pyo3 stalled 4 days past the available unblock.
- Out-of-band work reconciles cleanly against a frozen contract ONLY when its deliverables + evidence-protocol were precise enough to check after the fact.
- Split a drafted task AT THE CONTRACT (DRAFT-stage, nothing frozen) to ship the P0 half faster; carve the deferred half into a sibling with its grounding preserved.
- Repairing a never-green CI workflow: check FULL run history before calling a failure a "regression"; layered failures unmask one at a time — budget a triage round per layer + pre-agree the triage-or-split rule.
- Latent finding in an untouched file → triage fork: fix the stale-premise part in-scope (test tracks the dead assumption) + split the behavior part into a sibling task.
- Contract-freeze ⚠ flag + explicit human confirm is how a security-relevant default ships as a signed decision, not a silent risk (`/v1/scopes` cross-scope).
