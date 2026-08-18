# Local CI — the PR-time gate (CI diet, 2026-08-18)

## Why

Before the diet, one PR touching Rust + an SDK path queued up to seven
GitHub Actions workflows with a ~20-minute critical path (measured on the
GA-week PRs: conformance-bindings 19 min, mcp-prebuild 14, CI 12,
ts-prebuild 8, Integration 8, python-prebuild 7) — on shared runners,
before queue time and outage days. The same suites run in a fraction of
the time on a dev machine, and most of them were duplicated there anyway
before every push.

## The contract

| Where | What runs | When |
|---|---|---|
| **GitHub Actions, PR-time** | `ci.yml` (build + test + clippy, Linux — the cross-platform truth), CodeQL, Docs, perf-gates (only with the `perf-bench` label) | every PR |
| **Local, PR-time** | `scripts/ci/local-ci.sh` — fmt, clippy, full workspace tests, version parity; `full` adds SDK builds/tests, cargo-deny, optional LME ratchet | `quick` before every push; `full` before merging anything touching SDKs, storage, or retrieval |
| **GitHub Actions, post-merge** | Integration (Moon + moon-it), conformance-bindings, the three prebuild matrices, recall-ratchet | main-push, nightly cron, `workflow_dispatch` |
| **GitHub Actions, release** | prebuild publishes, crates-publish | `v*` tags |

Nothing lost coverage: every suite that left PR-time still runs on
main-push (so a bad merge is caught within one run), nightly (so a quiet
main is still re-verified), and on demand via dispatch. What changed is
*when* the wall-clock is paid and by which machine.

## Running it

```bash
scripts/ci/local-ci.sh            # quick tier
scripts/ci/local-ci.sh full       # + SDKs, cargo-deny, opt-in ratchet
LOCAL_CI_RATCHET=1 scripts/ci/local-ci.sh full
```

- `MOON_TEST_BINARY` must point at a moon ≥0.8.5 binary for the
  Moon-backed suites; the script defaults to
  `vendor/moon/target/release/moon` when built.
- The py/ts SDKs are excluded from `cargo test --workspace` (cdylib link
  errors) and tested via maturin+pytest / napi+vitest in the `full` tier —
  same shape as the conformance-bindings workflow.
- SKIP is not FAIL: missing local tooling (maturin, cargo-deny) skips the
  step loudly. Install the tool if the change you're shipping needs that
  gate locally, or lean on the post-merge Actions run.

## Rules of thumb

- Docs-only change: Actions' lean board is enough, no local run needed.
- Rust change: `local-ci.sh` (quick) before push.
- SDK / storage / retrieval change: `local-ci.sh full` before merge.
- Release tag: nothing changes — tags still run every publish matrix in
  Actions with attestation and registry idempotency.
