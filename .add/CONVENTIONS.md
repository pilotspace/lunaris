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
