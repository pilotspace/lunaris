# CI: Postgres (pg-lunaris) + Moon integration substrate

Operator reproduction guide + 8-minute budget runbook for
`.github/workflows/integration.yml`. Closes CI-04 + CI-05
(`.planning/REQUIREMENTS.md` §CI-XX, Phase 14 block of `.planning/ROADMAP.md`).

## 1. Why this exists

v0.1.1's HELIOS UAT (EVAL-05) surfaced 4 release-blocking bugs that
`postgres:16` in CI could never have caught — the vanilla substrate never
reaches the code paths those bugs live on. See
`.planning/milestones/v0.1.1-MILESTONE-AUDIT.md` for the full post-mortem.

Per REQUIREMENTS.md CI-03 (verbatim), the 4 bugs are:

1. **sqlx migration version collision**
2. **`SET search_path` session leak**
3. **candle `embed_tokens` tensor-path fallback for SentenceTransformer layout**
4. **bytea `LIKE` → byte-range scan on `lunaris_kv.key`**

Thesis: vanilla `postgres:16` in CI misses these paths; `pg-lunaris`
(pgvector + Apache AGE + pgmq) runs them. The `integration` job runs the
positive path against `pg-lunaris`; the sibling `integration-vanilla-pg-negative`
job proves each test would have caught its bug on the pre-v0.1.1 substrate.

## 2. Local reproduction

Boot **pg-lunaris** via the shipped compose file (mirrors the CI image byte-for-byte):

```bash
docker compose -f scripts/pg-lunaris/docker-compose.yml up --build -d
# Wait for readiness
for i in $(seq 1 30); do
  (echo > /dev/tcp/localhost/5432) >/dev/null 2>&1 && break
  sleep 2
done
```

Boot **Moon** from the pinned `vendor/moon` submodule (see
`scripts/moon-ci/README.md` for bump procedure + build-time budget; do not
duplicate commands here):

```bash
git submodule update --init --recursive -- vendor/moon
cargo build --release --manifest-path vendor/moon/Cargo.toml --bin moon
./vendor/moon/target/release/moon --port 6390 > /tmp/moon.log 2>&1 &
```

Run the conformance suite exactly as CI does:

```bash
MOON_URL=moon://localhost:6390 \
PG_URL=postgres://lunaris:lunaris@localhost:5432/lunaris \
  cargo test -p lunaris-conformance --features moon-it,pg-it --no-fail-fast
```

Env-var contract (identical to `.github/workflows/integration.yml` `env:` block):

| Var | Value |
|---|---|
| `PG_URL` | `postgres://lunaris:lunaris@localhost:5432/lunaris` |
| `MOON_URL` | `moon://localhost:6390` (CI port; local-dev commonly uses 6380) |

Teardown:

```bash
kill %1 2>/dev/null || true
docker compose -f scripts/pg-lunaris/docker-compose.yml down -v
```

## 3. Interpreting the 4 regression tests

Source of truth: `crates/lunaris-conformance/tests/regression.rs`
(`EXPECTED_VANILLA_ERRORS` const). The table below mirrors it; any bump
must update both places (P14-D06).

| # | Test file | Catches | pg-lunaris | vanilla pg:16 | `EXPECTED_VANILLA_ERROR` substring |
|---|---|---|---|---|---|
| 1 | `regression/sqlx_migration_version_collision.rs` | sqlx migration version collision | PASS | fails at first migration | `extension "vector" is not available` |
| 2 | `regression/search_path_session_leak.rs` | `SET search_path` session leak | PASS (negative invariant: 8 fresh checkouts do NOT return lunaris-only path) | fails at first migration | `extension "vector" is not available` |
| 3 | `regression/candle_embed_tokens_fallback.rs` | candle `embed_tokens` SentenceTransformer fallback | PASS (PG-independent) | PASS (PG-independent) | `None` |
| 4 | `regression/bytea_like_byterange_scan.rs` | bytea `LIKE` byte-range scan on `lunaris_kv.key` | PASS (EXPLAIN: `Index Scan` on `lunaris_kv_pkey`; no `Seq Scan`, no `~~`) | fails at first migration | `extension "vector" is not available` |

Honest framing: all 3 PG-dependent tests share the same substring because
vanilla pg:16 fails at the very first migration statement
(`CREATE EXTENSION IF NOT EXISTS vector`) — it never reaches the
bug-specific path. That IS the substrate-gap thesis CI-03 encodes.

How to read an `integration-vanilla-pg-negative` failure:

- **Substring-grep step fails** → vanilla-pg behavior changed. Update
  `EXPECTED_VANILLA_ERRORS` in `tests/regression.rs` AND the hardcoded
  mirror list in the negative-matrix job in `integration.yml`.
- **Sanity trip-wire step outcome == `success`** → the assertion logic
  itself may be silently green (R14-02). Investigate the trip-wire before
  trusting any PASS reports.

## 4. 8-minute budget runbook

Target: **p50 ≤ 480 s (8 min)** across ≥ 5 consecutive green main-branch
runs. Source: CI-04 acceptance. Wall-clock is harvested from the
`timing.json` artifact uploaded by the `integration` job (see the Budget
gate step).

Classification (from the `Budget gate (soft-fail)` step):

| Status | Range | Action |
|---|---|---|
| PASS | ≤ 480 s | None |
| WARN | 481–600 s | Watch next run; apply §4 tier 1 if repeats |
| BREACH | > 600 s | Apply §4 decision tree |

The gate is **soft-fail** — it classifies into `$GITHUB_STEP_SUMMARY` and
never fails the PR. Per R14-03 mitigation: budget breach triggers runbook
follow-up, not a red PR.

Decision tree on repeated BREACH (apply in order, cheapest first):

1. **Cache-miss rate** — check the `docker/build-push-action` step
   summary for the pg-lunaris build. If miss-rate > 50 %, the GHA cache
   may have been evicted (7-day policy); the next run re-warms.
2. **Moon build time** — check `Swatinem/rust-cache` summary for the
   `vendor/moon` workspace. Cold build is ~6 min (see
   `scripts/moon-ci/README.md`); a cold build with no SHA bump points at
   cache eviction.
3. **Test compile time** — `cargo test --no-run` warm-up. If > 2 min,
   consider `--test-threads=<n>` tuning on the positive-path step.
4. **Pinned base image** — per R14-03 mitigation: if tiers 1-3 are
   optimal and p50 still > 480 s, promote pg-lunaris to GHCR (deferred
   today; documented as the future optimization path, not a same-phase
   action).

## 5. Maintenance

- **Bump pgvector / AGE / pgmq**: edit `ARG` lines in
  `scripts/pg-lunaris/Dockerfile`, boot via `docker compose up --build`,
  verify extensions load, push PR.
- **Bump Moon**: follow `scripts/moon-ci/README.md` §Bump procedure. The
  submodule pointer at `vendor/moon` is the single source of truth — do
  NOT create a `MOON_CI_SHA` file.
- **Add a new regression test**: add a file under
  `crates/lunaris-conformance/tests/regression/`, add a row to
  `EXPECTED_VANILLA_ERRORS` in `tests/regression.rs`, add a matching
  per-test step + substring entry to the `integration-vanilla-pg-negative`
  job in `.github/workflows/integration.yml`. Commit all three together.

## 6. References

- `.planning/ROADMAP.md` Phase 14 block (success criteria)
- `.planning/REQUIREMENTS.md` §CI-XX (CI-01..05 acceptance)
- `.planning/milestones/v0.1.1-MILESTONE-AUDIT.md` (4-bug post-mortem)
- `crates/lunaris-conformance/tests/regression.rs` (`EXPECTED_VANILLA_ERRORS` const)
- `.github/workflows/integration.yml` (the pipeline)
- `scripts/pg-lunaris/docker-compose.yml` (local parity)
- `scripts/moon-ci/README.md` (Moon submodule bump + build-time budget)
