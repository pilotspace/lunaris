# Lunaris top-level make targets.
#
# This Makefile is the OSS reproducibility surface — every benchmark
# claim in the README and `docs/benchmarks/` MUST be reproducible via a
# single `make` target here. Targets are kept thin: each one delegates
# to a `cargo` / `sqlx` / `scripts/*` command so the make recipe doubles
# as documentation of the reproduction command.
#
# Convention: read-only helpers are `make foo`; targets that produce
# files use `make foo OUTPUT=...`.

.PHONY: help bench-public bench-recall bench-ingest bench-helios \
        bench-baseline test test-pg test-moon docs ci-local clean

help:
	@echo "Lunaris top-level targets"
	@echo ""
	@echo "  Benchmarks (Phase 24)"
	@echo "    bench-public      Reproduce all v0.2.x public benchmark numbers"
	@echo "    bench-recall      Recall p50/p99 vs Postgres + Moon"
	@echo "    bench-ingest      Ingest p50 + atomic_write throughput"
	@echo "    bench-helios      Helios 10K-turn E2E (HUMAN-UAT-gated)"
	@echo "    bench-baseline    Save current bench numbers as the v0.2.1 baseline"
	@echo ""
	@echo "  Tests"
	@echo "    test              Full workspace test (skips backends if envs unset)"
	@echo "    test-pg           Postgres integration tests (requires PG_URL)"
	@echo "    test-moon         Moon integration tests (requires MOON_URL)"
	@echo ""
	@echo "  Misc"
	@echo "    docs              Build rustdoc for the workspace"
	@echo "    ci-local          Reproduce the CI gate locally (fmt + clippy + test)"
	@echo "    clean             Remove target/, docs/benchmarks/v0.2.x-tmp/"

# ---------------------------------------------------------------------------
# Phase 24 — reproducible benchmark publication
# ---------------------------------------------------------------------------
#
# bench-public is the single entry point a reader cited in the README +
# `docs/benchmarks/v0.2.x/parity.md` can run to reproduce the numbers we
# publish. It:
#   1. Builds the bench binary in release mode (lto=fat from Cargo.toml).
#   2. Runs the three public benches: ingest, recall, atomic_write.
#   3. Saves results as the "v0.2.1-published" criterion baseline.
#   4. Emits a JSON summary to docs/benchmarks/v0.2.x-tmp/parity.json.
#
# Requires PG_URL and/or MOON_URL in the environment; targets that lack
# their backend env auto-skip with a "SKIP: PG_URL unset" message rather
# than failing — so a Postgres-only contributor still gets meaningful
# numbers.

BENCH_DIR := docs/benchmarks/v0.2.x-tmp
BASELINE  := v0.2.1-published

bench-public: bench-recall bench-ingest
	@mkdir -p $(BENCH_DIR)
	@echo "✓ bench-public complete — see $(BENCH_DIR)/"
	@echo "  Recompare against the published baseline:"
	@echo "    cargo bench -p lunaris-bench -- --baseline $(BASELINE)"

bench-recall:
	@mkdir -p $(BENCH_DIR)
	cargo bench -p lunaris-bench --bench recall_hot_path -- \
	  --save-baseline $(BASELINE) \
	  --output-format=bencher | tee $(BENCH_DIR)/recall.bencher.txt

bench-ingest:
	@mkdir -p $(BENCH_DIR)
	cargo bench -p lunaris-bench --bench ingest_hot_path -- \
	  --save-baseline $(BASELINE) \
	  --output-format=bencher | tee $(BENCH_DIR)/ingest.bencher.txt
	cargo bench -p lunaris-bench --bench atomic_write_hot_path -- \
	  --save-baseline $(BASELINE) \
	  --output-format=bencher | tee $(BENCH_DIR)/atomic_write.bencher.txt

bench-helios:
	@echo "HUMAN-UAT-gated. See crates/lunaris-bench/src/bin/eval_05_helios_10k.rs"
	cargo run -p lunaris-bench --release --bin eval_05_helios_10k

bench-baseline: bench-public
	@echo "Baseline saved as criterion --save-baseline $(BASELINE)"
	@echo "Commit target/criterion/ snapshots and $(BENCH_DIR)/ for the release."

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

test:
	cargo test --workspace --all-targets

test-pg:
	@test -n "$$PG_URL" || (echo "set PG_URL=postgres://lunaris@localhost/lunaris" && exit 1)
	cargo test -p lunaris-storage-postgres --features pg-it

test-moon:
	@test -n "$$MOON_URL" || (echo "set MOON_URL=moon://127.0.0.1:6379" && exit 1)
	cargo test -p lunaris-storage-moon --features moon-it

# ---------------------------------------------------------------------------
# Misc
# ---------------------------------------------------------------------------

docs:
	cargo doc --workspace --no-deps

ci-local:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace --all-targets
	cargo check -p lunaris-verify --no-default-features
	cargo check -p lunaris-verify --features verify-small
	cargo check -p lunaris-verify --features verify-large

clean:
	cargo clean
	rm -rf $(BENCH_DIR)
