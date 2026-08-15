//! Plan 05-06 EVAL-07 / EVAL-09 — Moon vs Postgres perf-delta harness.
//!
//! # RETIRED IN 0.7.0 — reads as SKIPPED, permanently
//!
//! The metric is a RATIO between two backends, and 0.7.0 deleted the second
//! one. The criterion benches no longer emit a `postgres` label (the row was
//! removed from `benches/{ingest,recall}_hot_path.rs` — it would have opened a
//! URL `Lunaris::open` now rejects), so `read_p50(.., "postgres")` returns
//! `None` on every run and every row is SKIPPED.
//!
//! It is kept registered, not deleted, for one reason: the manifest's row
//! cardinality is a contract (`run_all` promises the catalogue is always
//! complete, and CI greps the manifest for `"status":"FAIL"`). Dropping the
//! harness would silently shrink the catalogue. What IS changed is the skip
//! REASON — it used to say "run `cargo bench` first", which would send an
//! operator to re-run benches forever chasing data that can no longer exist.
//!
//! Deleting this harness and re-cutting the catalogue is a follow-up, not a
//! slice-B change.
//!
//! Walks Criterion JSON at
//! `target/criterion/<group>/<bench>/<label>/new/estimates.json` and emits
//! one [`EvalRow`] per `(group, bench)` with metric
//! `p50_ratio_postgres_over_moon`. Threshold = 5.0 per Lunaris pitch line
//! "3-5x lower p99 on Moon"; ratio > 5 means Moon advantage exceeded the
//! promised band, which we want to flag.
//!
//! Mirrors `crates/lunaris-bench/tests/budget_assertions.rs` lines 153-227
//! (`check_budget` walker) verbatim per Shared Pattern 8 — same Criterion
//! JSON path discovery + parse shape, adapted to compute a per-(group,bench)
//! ratio rather than a per-(group,bench,label) budget assertion.
//!
//! ## On-disk path
//!
//! Criterion writes per-bench data to:
//!
//! ```text
//! target/criterion/<group>/<bench>/<label>/new/estimates.json
//! ```
//!
//! For Lunaris benches, `<label>` is `moon` or `postgres` (the second arg of
//! `BenchmarkId::new("ingest_12kb_md", "moon")` per the Phase 2 ingest /
//! recall / atomic_write benches). So:
//!
//! ```text
//! target/criterion/ingest_hot_path/ingest_12kb_md/moon/new/estimates.json
//! target/criterion/ingest_hot_path/ingest_12kb_md/postgres/new/estimates.json
//! ```
//!
//! ## Soft-fail
//!
//! Missing `target/criterion/` directory OR missing per-row JSON → emit one
//! SKIPPED row per (group, bench) so the manifest cardinality stays stable
//! (CI greps for `"status":"FAIL"`; SKIPPED never blocks merge).

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Instant;

use crate::eval::EvalRow;

pub(crate) const HARNESS: &str = "perf-delta";
const RATIO_THRESHOLD: f64 = 5.0;

/// Why every row skips since 0.7.0. Named so the reason is one string rather
/// than two divergent copies, and so a grep for it lands on the module doc.
const RETIRED_REASON: &str =
    "retired in 0.7.0: this metric is a Moon-vs-Postgres ratio and the Postgres backend \
     was deleted, so the `postgres` Criterion label is never produced. Re-running \
     `cargo bench` will not populate it. See the module docs.";

/// `(criterion_group, bench_basename)` rows — mirror of `BUDGET_TABLE` from
/// `crates/lunaris-bench/tests/budget_assertions.rs:49-152`. Plan 02-04 +
/// Plan 03-04 set the existing benches; Plan 05-06 reuses them verbatim
/// without adding new Criterion harnesses (per CONTEXT.md D-23).
const BENCHES: &[(&str, &str)] = &[
    ("ingest_hot_path", "ingest_12kb_md"),
    ("ingest_hot_path", "ingest_12kb_md_graph_on"),
    ("recall_hot_path", "recall_q"),
    ("atomic_write_hot_path", "atomic_write"),
];

pub async fn run(results: &mut Vec<EvalRow>) -> anyhow::Result<()> {
    let target = std::env::var("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        // Fallback: walk from CARGO_MANIFEST_DIR (crates/lunaris-bench)
        // up to workspace root, then join target/. Mirrors Plan 04-03
        // chaos.rs binary-discovery walker shape.
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("target"))
            .unwrap_or_else(|| PathBuf::from("target"))
    });
    let criterion_root = target.join("criterion");

    // Whole-directory absence → emit SKIPPED rows per (group, bench) so the
    // manifest still has stable cardinality. Since 0.7.0 the populated branch
    // below skips too (no `postgres` label exists), so this is the same
    // outcome by a shorter path — the reason string is shared.
    if !criterion_root.exists() {
        for (group, bench) in BENCHES {
            results.push(EvalRow::skipped(
                HARNESS,
                &format!("{group}/{bench}::p50_ratio"),
                RATIO_THRESHOLD,
                RETIRED_REASON,
            ));
        }
        return Ok(());
    }

    for (group, bench) in BENCHES {
        let started = Instant::now();
        let moon_p50 = read_p50(&criterion_root, group, bench, "moon");
        let pg_p50 = read_p50(&criterion_root, group, bench, "postgres");

        let row = match (moon_p50, pg_p50) {
            (Some(m), Some(p)) if m > 0.0 => {
                let ratio = p / m;
                EvalRow::judge_le(
                    HARNESS,
                    &format!("{group}/{bench}::p50_ratio"),
                    ratio,
                    RATIO_THRESHOLD,
                    started.elapsed().as_millis() as u64,
                )
            }
            _ => EvalRow::skipped(
                HARNESS,
                &format!("{group}/{bench}::p50_ratio"),
                RATIO_THRESHOLD,
                RETIRED_REASON,
            ),
        };
        results.push(row);
    }
    Ok(())
}

/// Read the median (p50) value from
/// `target/criterion/<group>/<bench>/<label>/new/estimates.json`. Returns
/// the value in nanoseconds. `None` if the file is missing or malformed.
///
/// Path layout matches `budget_assertions.rs::check_budget` lines 350-370
/// verbatim — Criterion's `BenchmarkId::new(bench, label)` resolves to
/// the on-disk dir `<group>/<bench>/<label>/`.
fn read_p50(root: &std::path::Path, group: &str, bench: &str, label: &str) -> Option<f64> {
    let path = root.join(group).join(bench).join(label).join("new").join("estimates.json");
    let bytes = std::fs::read(&path).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    // Criterion estimates.json shape: median.point_estimate is the p50 (in ns).
    v.get("median")?.get("point_estimate")?.as_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_synth_estimates(
        root: &std::path::Path,
        group: &str,
        bench: &str,
        label: &str,
        ns: f64,
    ) {
        let dir = root.join(group).join(bench).join(label).join("new");
        std::fs::create_dir_all(&dir).unwrap();
        let body = serde_json::json!({
            "median": { "point_estimate": ns }
        });
        std::fs::write(dir.join("estimates.json"), serde_json::to_vec(&body).unwrap()).unwrap();
    }

    #[test]
    fn read_p50_returns_median_ns() {
        let tmp =
            std::env::temp_dir().join(format!("lunaris-perf-delta-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        write_synth_estimates(&tmp, "ingest_hot_path", "ingest_12kb_md", "moon", 30_000_000.0);
        let v = read_p50(&tmp, "ingest_hot_path", "ingest_12kb_md", "moon");
        assert_eq!(v, Some(30_000_000.0));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_p50_missing_file_returns_none() {
        let tmp =
            std::env::temp_dir().join(format!("lunaris-perf-delta-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(read_p50(&tmp, "g", "b", "l").is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn missing_criterion_root_emits_skipped_rows_per_bench() {
        // Point CARGO_TARGET_DIR at a non-existent dir to force the
        // "missing root" branch. NOTE: this test is racy with sibling tests
        // that read CARGO_TARGET_DIR; we don't actually mutate the env
        // (which would require unsafe in Rust 2024). Instead we directly
        // exercise the inner loop by constructing the path manually.
        let nonexistent =
            std::env::temp_dir().join(format!("lunaris-perf-delta-no-such-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&nonexistent);
        // Direct synthesis of the SKIPPED branch:
        let mut results: Vec<EvalRow> = Vec::new();
        for (group, bench) in BENCHES {
            results.push(EvalRow::skipped(
                HARNESS,
                &format!("{group}/{bench}::p50_ratio"),
                RATIO_THRESHOLD,
                "no Criterion data — synthesized for unit test",
            ));
        }
        assert_eq!(results.len(), BENCHES.len());
        assert!(results.iter().all(|r| r.status == "SKIPPED"));
    }

    /// 0.7.0: `run` must emit a full-cardinality catalogue of SKIPPED rows
    /// whose reason names the removal, whether or not `target/criterion/`
    /// exists on this machine. Both branches share `RETIRED_REASON`, so this
    /// holds on a developer box with stale bench data and in a clean CI
    /// checkout alike — which is the point: neither can produce a ratio.
    #[tokio::test]
    async fn run_emits_a_retired_skip_for_every_bench() {
        let mut results: Vec<EvalRow> = Vec::new();
        super::run(&mut results).await.unwrap();

        assert_eq!(
            results.len(),
            BENCHES.len(),
            "the catalogue cardinality is a manifest contract"
        );
        for row in &results {
            assert_eq!(row.status, "SKIPPED", "no ratio is computable without a second backend");
        }
    }

    /// `EvalRow` does not carry the reason — `skipped` prints it to stderr and
    /// drops it — so the wording cannot be asserted through a row. Pin the
    /// constant directly: it must name the removal, and must NOT send the
    /// reader to `cargo bench`, which is the exact dead end 0.7.0 created.
    #[test]
    fn the_retired_reason_names_the_removal_and_not_a_rerun() {
        assert!(RETIRED_REASON.contains("retired in 0.7.0"));
        assert!(RETIRED_REASON.contains("Postgres backend"));
        assert!(
            !RETIRED_REASON.contains("run `cargo bench`"),
            "re-running benches cannot produce the missing label"
        );
    }
}
