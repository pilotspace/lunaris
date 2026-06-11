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
