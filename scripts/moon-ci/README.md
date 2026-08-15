# Moon-in-CI: submodule mechanism (Plan 14-02)

This directory is the operator runbook for the **Moon** source that the
`.github/workflows/integration.yml` `integration` job builds from source to
provide a live Moon service for `moon-it` + `pg-it` integration tests
(REQ CI-02).

## Mechanism: git submodule at `vendor/moon`

Per Phase 14-02 Task 1 decision (**Option B**), Moon lives in this repository as
a pinned git submodule at `vendor/moon`. No external secret (`MOON_CI_TOKEN`) is
required — the submodule is resolved via `actions/checkout@v4` with
`submodules: recursive` using the job's default `GITHUB_TOKEN` (or the user's
SSH key for local clones).

The pinned SHA is recorded **exclusively** by the submodule pointer in git's
index (visible via `git ls-tree HEAD vendor/moon`) and by the `[submodule "vendor/moon"]`
stanza in `.gitmodules`. There is intentionally **no** `MOON_CI_SHA` file — the
submodule pointer is the single source of truth.

## Bump procedure (operator)

To move the CI-built Moon binary forward to a newer Moon commit:

```bash
# 1. Fetch the newest Moon refs into the submodule
git -C vendor/moon fetch origin

# 2. Check out the target SHA (prefer a tagged release once Moon tags them)
git -C vendor/moon checkout <new-sha>

# 3. Stage the submodule pointer bump in the parent repo
git add vendor/moon

# 4. Commit with a scoped message
git commit -m "chore(moon-ci): bump vendor/moon to <short-sha>"
```

Reviewers confirm the bump by reading `git diff HEAD^ HEAD -- vendor/moon`
(single-line subproject commit pointer change) and by watching the
`integration` job's Moon build step on the PR.

## Build-time budget

Observed wall-clock on `ubuntu-latest` with `Swatinem/rust-cache@v2` keyed on
the pinned Moon SHA:

| Cache state | `cargo build --release --manifest-path vendor/moon/Cargo.toml --bin moon` |
|---|---|
| Cold (first run of a bumped SHA) | **< 8 min** target; hand-run baseline: ~6 min on a 4-core 2024-era runner (seed observation 2026-04-23) |
| Warm (SHA unchanged) | **< 3 min** target; `Swatinem/rust-cache@v2` hits `vendor/moon/target` |

Budget breach triggers the Plan 14-04 runbook (pinned base image, cache-key
audit). Breach does NOT trigger a mechanism change (Option A / Option C are
documented below as future alternatives, not fallbacks).

## Local reproduction (operator)

```bash
# From the lunaris repo root with the submodule initialized
git submodule update --init --recursive -- vendor/moon

# Build Moon (same command as CI)
cargo build --release --manifest-path vendor/moon/Cargo.toml --bin moon

# Launch Moon on the CI port (6390), backgrounded with stdout redirected
mkdir -p "${RUNNER_TEMP:-/tmp}"
./vendor/moon/target/release/moon --port 6390 \
  > "${RUNNER_TEMP:-/tmp}/moon.log" 2>&1 &
MOON_PID=$!

# Wait for Moon to be reachable
for i in $(seq 1 30); do
  if (echo > /dev/tcp/localhost/6390) >/dev/null 2>&1; then
    echo "moon reachable after $i attempts"
    break
  fi
  sleep 2
done

# Run the conformance suite against the live Moon. (Through 0.6.x this ran
# both backends via `--features moon-it,pg-it` against a `pg-lunaris`
# container; the Postgres backend, the `pg-it` feature, and the image were all
# deleted in 0.7.0.)
MOON_URL=moon://localhost:6390 \
  cargo test -p lunaris-conformance --features moon-it --no-fail-fast

# Cleanup
kill $MOON_PID || true
```

## Future alternatives (NOT active)

TODO — bump to a tagged release once Moon starts publishing them. Preferred
path is to transition from `vendor/moon` submodule to:

- **Option C (pinned GitHub Release artifact):** `gh release download --repo pilotspace/moon <tag>` — fastest CI boot, no build step. Blocked today (2026-04-23) because no public Moon releases exist.
- **Option A (secondary checkout with `MOON_CI_TOKEN`):** retained as an escape hatch if the submodule approach ever conflicts with a policy on external-repo submodules. Requires a repo secret.

Neither is active today; this document is the canonical Moon-source mechanism
until a planner explicitly flips one of the above on.

## Cross-references

- Plan: `.planning/phases/14-ci-substrate-hardening/14-02-PLAN.md`
- Workflow: `.github/workflows/integration.yml` (`integration` job, Moon build + service steps)
- REQ: `.planning/REQUIREMENTS.md` CI-02
- Local-dev Moon launch (port 6380): user memory `reference_moon_local_run.md` — note the **port differs** between local-dev (6380) and CI (6390, to match `eval-gauntlet.yml`'s URL contract).
