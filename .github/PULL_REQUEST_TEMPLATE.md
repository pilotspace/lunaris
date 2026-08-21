## What this changes

<!-- One paragraph. What behaviour is different after this merges? -->

## Why

<!-- The problem, not the patch. Link the issue if there is one. -->

## How it was verified

<!--
Required. "CI is green" is not a verification — say what you ran and what it
proved. Paste the command and the relevant output.

  - New/changed tests, and what they would catch if the fix regressed
  - For anything on a production path: the test that proves the PRODUCTION
    path invokes it (built is not wired)
  - For storage-backed work: MOON_TEST_BINARY=... cargo test -p <crate>
-->

## Evidence for any number

<!--
Delete this section if the PR adds no numbers. Otherwise: every figure in the
diff must trace to a committed artifact — a report under docs/benchmarks/ with
its raw samples, or docs/operations/capacity.md. State the configuration
(corpus size, k, graph on/off, rerank on/off, hardware) with the number.
Numbers without a source get deleted, not softened.
-->

## Checklist

- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean
- [ ] `cargo test --workspace --all-targets --exclude lunaris-py --exclude lunaris-ts` passes
- [ ] Formatted with the workspace-member `cargo fmt` invocation (**not** `cargo fmt --all`)
- [ ] Red and green are separate commits where a behaviour changed
- [ ] Docs updated in the same PR if a public surface or a claim changed
- [ ] `CHANGELOG.md` updated if this is user-visible

## Contract impact

- [ ] Does **not** add a second `atomic_write` to an ingest path (INGEST-04)
- [ ] Does **not** hold a lock across `.await`
- [ ] Does **not** put an LLM call on the recall hot path
- [ ] New MCP `#[tool]` response DTOs are flat structs (schema root `type: "object"`)
- [ ] `embedded-moon` is still absent from every default feature set
- [ ] Public request DTOs carry `#[serde(deny_unknown_fields)]`

<!-- Anything you ticked "no" on: explain below. -->
