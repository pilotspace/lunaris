//! Blueprint §4.1 / §4.2 latency budget enforcement — Moon-only.
//!
//! Reads Criterion's `target/criterion/<group>/<bench>_<label>/new/{estimates,sample}.json`
//! and asserts the measured p50 + p99 satisfy the budget contract. Failure
//! messages carry actionable detail
//! (`"ingest_12kb_md/moon p50 73.0 ms > 50.0 ms budget — over by +46%"`).
//!
//! ## Moon-only since 0.7.0 (re-derived — ship-plan W4.9)
//!
//! The table shipped 10 rows: 5 Moon and 5 Postgres, plus a soft-fail rule
//! that downgraded a Postgres row missing budget by >2× to a `tracing::warn!`.
//! 0.7.0 deleted the Postgres backend. `PG_URL` is set by nothing, the scheme
//! is rejected, and every Postgres row would resolve to SKIP forever — so
//! half the contract graded nothing and the soft-fail branch was unreachable.
//! Both are gone. Five Moon rows remain, and `the_table_names_no_retired_backend`
//! keeps it that way.
//!
//! ## An empty sweep is not a passing sweep
//!
//! The budgets are read off disk, so with no Criterion data every row skips,
//! there is nothing left to fail on, and the suite reports success having
//! measured nothing. That was its entire history: it is gated behind
//! `--features budget-it`, no workflow passed that feature, and no workflow
//! ran the four benches it reads. [`Sweep::verdict`] now fails on a sweep that
//! measured nothing, and on a sweep that measured only some rows — a row that
//! cannot be measured where the gate runs does not belong in the table.
//!
//! ## How to run
//!
//! ```bash
//! # 1. A single-shard Moon on the bench port (NEVER 6379/6380/6381).
//! moon --port 6399 --shards 1 --dir /tmp/moon-bench &
//!
//! # 2. Populate Criterion data. The 1M-fact recall corpus is a one-time
//! #    ~5-10 min build per store, cached by fingerprint afterwards.
//! MOON_URL=moon://127.0.0.1:6399 cargo bench -p lunaris-bench \
//!   --bench ingest_hot_path --bench recall_hot_path \
//!   --bench atomic_write_hot_path --bench helios_p50
//!
//! # 3. Enforce.
//! cargo test -p lunaris-bench --features budget-it --test budget_assertions
//! ```
//!
//! Step 3 reads `$CARGO_TARGET_DIR/criterion`, so it must run with the same
//! `CARGO_TARGET_DIR` as step 2.

#![cfg(feature = "budget-it")]

use std::fs;
use std::path::{Path, PathBuf};

// ----- budget table -----
//
// Each row: (group, bench_id, label, p50_budget_ns, p99_budget_ns).
// `label` is always "moon" — see `the_table_names_no_retired_backend`.
// p99 = 0 means "no p99 budget for this row; skip the p99 check".

const BUDGET_TABLE: &[BudgetRow] = &[
    // INGEST-05 — ingest p50 ≤ 50 ms / p99 ≤ 110 ms (12 KB markdown).
    BudgetRow {
        group: "ingest_hot_path",
        bench: "ingest_12kb_md",
        label: "moon",
        p50_budget_ns: 50_000_000,
        p99_budget_ns: 110_000_000,
        known_miss: None,
    },
    // RETRIEVE-11 — recall p50 ≤ 25 ms / p99 ≤ 80 ms, 1M facts. This is the
    // row CLAUDE.md calls the core value contract.
    BudgetRow {
        group: "recall_hot_path",
        bench: "recall_q",
        label: "moon",
        p50_budget_ns: 25_000_000,
        p99_budget_ns: 80_000_000,
        // Measured 2026-08-25 at p50 261.15 ms / p99 274.04 ms on the CI
        // runner. Ceilings sit ~1.5x above that: high enough not to flake on a
        // slower runner, low enough that a real regression still lands outside.
        known_miss: Some(KnownMiss {
            tracked_as: "pilotspace/moon#718 (see also W4.9 in the ship plan)",
            p50_ceiling_ns: 400_000_000,
            p99_ceiling_ns: 450_000_000,
            why: "measured over a 1M-row index that is never compacted — Moon 0.8.5's merge \
                  verifier returns 0.0000 forever, and our own MOON_MAX_UNFLUSHED_SEGMENTS=0 \
                  workaround removes the backpressure that would otherwise stop the bench, so \
                  every KNN fans out over 20+ unmerged segments on a core-0-pinned shared \
                  runner. This number is NOT yet evidence of a recall regression. Delete this \
                  entry after re-measuring on a compacted index; if it is still over, that is a \
                  real finding and belongs in its own issue.",
        }),
    },
    // Blueprint §4.1 atomic_write — 3 ms p50 / 12 ms p99.
    BudgetRow {
        group: "atomic_write_hot_path",
        bench: "atomic_write",
        label: "moon",
        p50_budget_ns: 3_000_000,
        p99_budget_ns: 12_000_000,
        known_miss: None,
    },
    // INGEST-06 — graph-on ingest p50 ≤ 300 ms / p99 ≤ 570 ms (blueprint §4.1).
    BudgetRow {
        group: "ingest_hot_path",
        bench: "ingest_12kb_md_graph_on",
        label: "moon",
        p50_budget_ns: 300_000_000,
        p99_budget_ns: 570_000_000,
        // Measured 2026-08-25 on two platforms. p50 is a tight cluster —
        // 863.51 ms on the Linux CI runner, 869.86 ms on macOS — which is what
        // says this is the code and not the runner. p99 is not: 1328.05 ms on
        // CI against 2025.15 ms on macOS. Ceilings are set ~1.4x above the
        // WORST observation on either platform, not above the first one
        // measured; a ceiling drawn from a single machine is a flake waiting
        // for the other machine to run it.
        known_miss: Some(KnownMiss {
            tracked_as: "pilotspace/moon#719",
            p50_ceiling_ns: 1_200_000_000,
            p99_ceiling_ns: 3_000_000_000,
            why: "Moon's Cypher WRITE executor ignores the property index, so the create-path \
                  MATCH…SET scans every node of the label and graph-node creation is O(N^2). \
                  NOT a stale budget: the blueprint's 5-chunk assumption re-measures as exactly \
                  5 through the production path, and the payload is a constant 50 entities / \
                  150 relations. No application-side workaround exists — see \
                  crates/lunaris-storage-moon/tests/graph_write_scan_reverse_ratchet.rs, which \
                  fails the day this is fixed and lists the re-measure steps.",
        }),
    },
    // HELIOS-05 (CONTEXT.md D-07/D-09) — p50 ≤ 20 ms tool-call overhead for
    // the scratchpad surface. `p99_budget_ns = 0` per D-09: p99 was never
    // tightened by Phase 12 and there is no longer a second backend to
    // inherit a p99 budget from, so this row is p50-only by construction.
    BudgetRow {
        group: "helios_smoke",
        bench: "helios_p50",
        label: "moon",
        p50_budget_ns: 20_000_000,
        p99_budget_ns: 0,
        known_miss: None,
    },
];

/// A row that misses its blueprint budget for a reason we have diagnosed and
/// cannot fix from this repo.
///
/// This is a ratchet, not a waiver, and the difference is the whole point. A
/// plain exception list only ever grows: rows get added when they go red and
/// nothing ever takes them out, so the gate quietly stops covering the thing
/// it was built for. Three rules prevent that:
///
/// * The row must still be MEASURED. A known miss is never a skip.
/// * It must stay under `p50_ceiling_ns` / `p99_ceiling_ns`, which sit above
///   the measurement that justified the entry — not above the budget. A row
///   that gets worse still fails, so an exception cannot absorb a fresh
///   regression.
/// * If it comes back INSIDE its real budget the sweep FAILS and says to
///   delete the entry. An exception that outlived its cause is exactly the
///   rubber stamp this design exists to prevent.
#[derive(Debug, Clone, Copy)]
struct KnownMiss {
    /// Where the cause is tracked. Required — an exception with no owner is
    /// indistinguishable from one nobody ever revisited.
    tracked_as: &'static str,
    /// Upper bound on p50, set above the measurement that justified the entry.
    p50_ceiling_ns: u64,
    /// Upper bound on p99. ZERO means "not ratcheted", legitimate only for a
    /// row that has no p99 budget in the first place.
    p99_ceiling_ns: u64,
    /// What is wrong, and what would let this entry be deleted.
    why: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct BudgetRow {
    group: &'static str,
    bench: &'static str,
    label: &'static str,
    /// p50 budget in nanoseconds. NEVER zero — every row has a p50 budget.
    p50_budget_ns: u64,
    /// p99 budget in nanoseconds. ZERO means "no p99 budget for this row;
    /// skip the p99 check". Only `helios_smoke::helios_p50` uses this escape:
    /// CONTEXT.md D-09 left its p99 untightened, and the Postgres row it would
    /// have inherited a p99 from no longer exists.
    p99_budget_ns: u64,
    /// `Some` when this row is a diagnosed, tracked miss. See [`KnownMiss`].
    known_miss: Option<KnownMiss>,
}

#[derive(Debug, Clone)]
struct BudgetReport {
    group: String,
    bench: String,
    label: String,
    p50_ms: f64,
    p99_ms: f64,
    p50_budget_ms: f64,
    p99_budget_ms: f64,
    p50_within_budget: bool,
    p99_within_budget: bool,
}

#[derive(Debug)]
enum CheckOutcome {
    Skipped(String),
    Reported(BudgetReport),
}

// ----- assertions -----

/// The result of walking [`BUDGET_TABLE`] against one Criterion root.
///
/// Split out of the test body so the "measured nothing" case is reachable
/// from a unit test with a synthetic root. While this logic lived inline,
/// the only way to exercise it was to have real bench data on disk — which
/// is precisely the situation it exists to detect the absence of.
#[derive(Debug, Default)]
struct Sweep {
    passed: Vec<String>,
    skipped: Vec<String>,
    hard_failures: Vec<String>,
    /// Rows outside budget for a diagnosed, tracked cause and no worse than
    /// the ceiling recorded with it. These are MEASURED and reported, never
    /// skipped — the number still has to be produced every run, which is what
    /// makes the ceiling and the obsolete-entry check enforceable.
    known_misses: Vec<String>,
}

impl Sweep {
    /// Rows that produced a real number. A row that skipped is NOT measured.
    fn measured(&self) -> usize {
        self.passed.len() + self.hard_failures.len() + self.known_misses.len()
    }

    /// `Ok(())` only when every row in the table produced a number and every
    /// number was inside budget.
    ///
    /// The zero-measurement arm is the load-bearing one. Criterion data is
    /// read off disk, so a sweep with no data on disk skips every row and has
    /// nothing to fail on — and a test that fails on nothing passes.
    fn verdict(&self) -> Result<(), String> {
        if self.measured() == 0 {
            return Err(format!(
                "budget sweep measured NOTHING — {} row(s) skipped, 0 measured.\n\
                 An empty sweep is not a passing sweep. Populate Criterion data first:\n  \
                 MOON_URL=moon://127.0.0.1:6399 cargo bench -p lunaris-bench \\\n    \
                 --bench ingest_hot_path --bench recall_hot_path \\\n    \
                 --bench atomic_write_hot_path --bench helios_p50\n\
                 Skipped rows:\n{}",
                self.skipped.len(),
                self.skipped.join("\n")
            ));
        }
        if !self.skipped.is_empty() {
            return Err(format!(
                "budget sweep is INCOMPLETE — {} of {} rows had no data.\n\
                 Every row in BUDGET_TABLE must be measurable in the environment that runs\n\
                 this gate; a row nobody can measure does not belong in the table.\n{}",
                self.skipped.len(),
                BUDGET_TABLE.len(),
                self.skipped.join("\n")
            ));
        }
        if !self.hard_failures.is_empty() {
            return Err(format!(
                "latency budget enforcement FAILED — {} miss(es):\n{}",
                self.hard_failures.len(),
                self.hard_failures.join("\n")
            ));
        }
        Ok(())
    }
}

/// How one measured row lands against its budget and any [`KnownMiss`] entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowVerdict {
    /// Inside its blueprint budget, with no exception recorded.
    Pass,
    /// Outside its budget, but diagnosed, tracked, and no worse than the
    /// ceiling recorded with the exception.
    KnownMiss,
    /// Inside its budget WHILE carrying an exception — the exception has
    /// outlived its cause and must be deleted.
    ObsoleteWaiver,
    /// Outside its budget with no exception, or outside the ceiling of the
    /// exception it has.
    Fail,
}

/// Classify one measured row. Pure, so the four outcomes can be driven
/// directly instead of through Criterion data on disk.
fn classify_row(row: &BudgetRow, rep: &BudgetReport) -> RowVerdict {
    let within_budget = rep.p50_within_budget && rep.p99_within_budget;
    match row.known_miss {
        None => {
            if within_budget {
                RowVerdict::Pass
            } else {
                RowVerdict::Fail
            }
        }
        Some(km) => {
            if within_budget {
                // The cause is fixed. Say so loudly rather than leaving a
                // dead entry that quietly narrows the gate forever.
                return RowVerdict::ObsoleteWaiver;
            }
            let p50_ok = rep.p50_ms * 1e6 <= km.p50_ceiling_ns as f64;
            // A zero p99 ceiling means "not ratcheted", which is only
            // legitimate where there is no p99 budget to miss in the first
            // place. Where there IS one, a zero ceiling must not read as
            // "anything goes".
            let p99_ok = if km.p99_ceiling_ns == 0 {
                row.p99_budget_ns == 0
            } else {
                rep.p99_ms * 1e6 <= km.p99_ceiling_ns as f64
            };
            if p50_ok && p99_ok { RowVerdict::KnownMiss } else { RowVerdict::Fail }
        }
    }
}

/// Walk [`BUDGET_TABLE`] against `target_root` and classify every row.
fn sweep_budgets(target_root: &Path) -> Sweep {
    let mut sweep = Sweep::default();
    for row in BUDGET_TABLE {
        match check_budget(target_root, *row) {
            Ok(CheckOutcome::Skipped(why)) => {
                sweep
                    .skipped
                    .push(format!("{}/{}/{} → SKIP ({why})", row.group, row.bench, row.label));
            }
            Ok(CheckOutcome::Reported(rep)) => {
                let id = format!("{}/{}/{}", rep.group, rep.bench, rep.label);
                match classify_row(row, &rep) {
                    RowVerdict::KnownMiss => {
                        let km = row.known_miss.expect("KnownMiss verdict implies an entry");
                        sweep.known_misses.push(format!(
                            "{id} → {} [tracked: {}; ceiling p50 {:.0} ms / p99 {:.0} ms] {}",
                            format_failure(&rep),
                            km.tracked_as,
                            km.p50_ceiling_ns as f64 / 1e6,
                            km.p99_ceiling_ns as f64 / 1e6,
                            km.why,
                        ));
                        continue;
                    }
                    RowVerdict::ObsoleteWaiver => {
                        let km = row.known_miss.expect("ObsoleteWaiver implies an entry");
                        sweep.hard_failures.push(format!(
                            "{id} → now INSIDE budget (p50 {:.2} ms ≤ {:.2} ms) while still \
                             carrying a known-miss entry for {}. The cause appears fixed — \
                             delete the `known_miss` on this row so the budget is enforced \
                             again. ({})",
                            rep.p50_ms, rep.p50_budget_ms, km.tracked_as, km.why,
                        ));
                        continue;
                    }
                    RowVerdict::Pass | RowVerdict::Fail => {}
                }
                if rep.p50_within_budget && rep.p99_within_budget {
                    // A row with no p99 contract has `p99_budget_ms == 0`, and
                    // rendering that as "p99 2.18 ms ≤ 0.00 ms" on a PASS line
                    // reads as a gate that checked something impossible and
                    // shrugged. Say what is actually true instead.
                    let p99 = if rep.p99_budget_ms == 0.0 {
                        format!("p99 {:.2} ms (no p99 contract for this row)", rep.p99_ms)
                    } else {
                        format!("p99 {:.2} ms ≤ {:.2} ms", rep.p99_ms, rep.p99_budget_ms)
                    };
                    sweep.passed.push(format!(
                        "{id} → p50 {:.2} ms ≤ {:.2} ms; {p99}",
                        rep.p50_ms, rep.p50_budget_ms
                    ));
                } else {
                    sweep.hard_failures.push(format!("{id} → {}", format_failure(&rep)));
                }
            }
            Err(e) => {
                sweep
                    .hard_failures
                    .push(format!("{}/{}/{} → check error: {e}", row.group, row.bench, row.label));
            }
        }
    }
    sweep
}

/// Walks [`BUDGET_TABLE`] and fails when any row misses its blueprint
/// §4.1/§4.2 budget — or when the sweep had nothing to measure.
#[test]
fn enforces_moon_latency_budgets() {
    init_tracing();
    let sweep = sweep_budgets(&criterion_root());
    print_report(&sweep);
    if let Err(msg) = sweep.verdict() {
        panic!("{msg}");
    }
}

/// Shape check for the INGEST-06 graph-on row. Runs without live data, so
/// CI catches a table drift even when no backend is reachable.
#[test]
fn budget_table_has_the_ingest_06_graph_on_row() {
    let moon_row = BUDGET_TABLE
        .iter()
        .find(|r| {
            r.group == "ingest_hot_path"
                && r.bench == "ingest_12kb_md_graph_on"
                && r.label == "moon"
        })
        .expect("INGEST-06 Moon row must exist");
    assert_eq!(moon_row.p50_budget_ns, 300_000_000, "blueprint §4.1 graph-on p50 = 300 ms");
    assert_eq!(moon_row.p99_budget_ns, 570_000_000, "blueprint §4.1 graph-on p99 = 570 ms");

    // 0.7.0 is Moon-only. The row count is pinned so a re-added backend row
    // has to come with a deliberate edit here, and the grep gate
    // `grep -c 'BUDGET_TABLE.len(), 5'` finds it verbatim.
    assert_eq!(BUDGET_TABLE.len(), 5, "budget table row count (Moon-only since 0.7.0)");
}

/// 0.7.0 deleted the Postgres backend. A `postgres` row cannot be measured —
/// `PG_URL` is never set and the URL scheme is rejected — so every such row
/// would resolve to SKIP forever. Under the sweep's completeness rule that is
/// no longer a quiet no-op: it fails the gate. Pin the absence directly so
/// the reason is stated once, here, rather than rediscovered from a
/// confusing skip.
#[test]
fn the_table_names_no_retired_backend() {
    let retired: Vec<&str> =
        BUDGET_TABLE.iter().map(|r| r.label).filter(|l| *l != "moon").collect();
    assert!(retired.is_empty(), "0.7.0 is Moon-only, but BUDGET_TABLE still names: {retired:?}");
}

/// HELIOS-05 shape check — the `helios_smoke::helios_p50` row sits at exactly
/// the 20 ms p50 CONTEXT.md D-07 specifies, with `p99_budget_ns == 0` per D-09
/// (p99 intentionally untightened). Runs without live data.
#[test]
fn verifies_helios_budget_row_present() {
    let moon_row = BUDGET_TABLE
        .iter()
        .find(|r| r.group == "helios_smoke" && r.bench == "helios_p50" && r.label == "moon")
        .expect("HELIOS-05 Moon row must exist");
    assert_eq!(
        moon_row.p50_budget_ns, 20_000_000,
        "HELIOS-05 / CONTEXT.md D-07: p50 budget = 20 ms"
    );
    assert_eq!(
        moon_row.p99_budget_ns, 0,
        "HELIOS-05 / CONTEXT.md D-09: p99 intentionally untightened"
    );
}

/// Populate synthetic Criterion data for every row in [`BUDGET_TABLE`],
/// each at `fraction` of its own p50 budget.
///
/// The p99 sample is written at the same fraction of the p99 budget, except
/// for rows with `p99_budget_ns == 0` (no p99 contract), which get a sample
/// derived from p50 so the parse path still has something to read.
fn populate_all_rows(root: &Path, fraction: f64) {
    for row in BUDGET_TABLE {
        // A row carrying a `known_miss` is EXPECTED to be over budget, so
        // populating it at a fraction of its budget would render it as an
        // obsolete entry and fail the sweep. Scale such rows against their
        // ceiling instead, which is the state "everything is as we recorded
        // it" for that row. `every_known_miss_in_the_table_names_a_tracking_
        // reference` pins ceiling > budget, so a fraction near 0.5 lands
        // between the two — a genuine known miss, not an accidental pass.
        let scale_ns = match row.known_miss {
            Some(km) => km.p50_ceiling_ns,
            None => row.p50_budget_ns,
        } as f64;
        let p50 = scale_ns * fraction;
        write_synthetic_estimates(root, row.group, row.bench, row.label, p50);
        let p99_scale_ns = match row.known_miss {
            Some(km) if km.p99_ceiling_ns > 0 => km.p99_ceiling_ns,
            _ => row.p99_budget_ns,
        };
        let top = if p99_scale_ns > 0 { p99_scale_ns as f64 * fraction } else { p50 * 1.2 };
        write_synthetic_sample(
            root,
            row.group,
            row.bench,
            row.label,
            &[p50 * 0.9, p50, p50 * 1.05, top * 0.95, top],
        );
    }
}

/// The gate's reason for existing.
///
/// With no Criterion data on disk every row skips, so there is nothing left
/// to fail on — and a test that fails on nothing passes. This suite reported
/// exactly that for its entire life, while no workflow ran the benches it
/// reads. A sweep that measured nothing must not be a pass.
#[test]
fn a_sweep_that_measured_nothing_is_not_a_pass() {
    let empty = tempdir_for_test("budget_sweep_no_data");
    let sweep = sweep_budgets(&empty);

    // Fixture validity first: prove the sweep really did measure nothing,
    // so a later Err cannot be mistaken for the guard firing on real data.
    assert_eq!(sweep.measured(), 0, "fixture: an empty root must measure nothing");
    assert_eq!(sweep.skipped.len(), BUDGET_TABLE.len(), "fixture: every row must skip");

    let verdict = sweep.verdict();
    let err = verdict.expect_err("a sweep that measured nothing must not report success");
    assert!(err.contains("measured NOTHING"), "message must name the cause: {err}");
    assert!(err.contains("cargo bench"), "message must say how to fix it: {err}");
}

/// Half a sweep is the same disease in a smaller dose: the rows with data
/// pass, the rows without quietly vanish, and the gate reports green over a
/// contract it only partly checked.
#[test]
fn a_partial_sweep_is_not_a_pass() {
    let root = tempdir_for_test("budget_sweep_partial");
    let row = BUDGET_TABLE[0];
    write_synthetic_estimates(
        &root,
        row.group,
        row.bench,
        row.label,
        row.p50_budget_ns as f64 * 0.5,
    );
    write_synthetic_sample(
        &root,
        row.group,
        row.bench,
        row.label,
        &[row.p50_budget_ns as f64 * 0.5, row.p99_budget_ns as f64 * 0.5],
    );

    let sweep = sweep_budgets(&root);
    assert_eq!(sweep.measured(), 1, "fixture: exactly one row has data");
    assert_eq!(sweep.hard_failures.len(), 0, "fixture: the measured row is inside budget");

    let err = sweep.verdict().expect_err("an incomplete sweep must not report success");
    assert!(err.contains("INCOMPLETE"), "message must name the cause: {err}");
    assert!(err.contains("recall_hot_path"), "message must name a row that had no data: {err}");
}

/// The discriminating positive. Without it, a `verdict` hard-wired to `Err`
/// would satisfy both tests above — "never passes" and "passes only when the
/// contract is met" are indistinguishable until something is asked to pass.
#[test]
fn a_complete_in_budget_sweep_passes() {
    let root = tempdir_for_test("budget_sweep_complete_ok");
    populate_all_rows(&root, 0.5);

    let sweep = sweep_budgets(&root);
    assert_eq!(sweep.measured(), BUDGET_TABLE.len(), "fixture: every row measured");
    assert_eq!(sweep.skipped.len(), 0, "fixture: nothing skipped");
    // Fixture validity: the waived rows must land as KNOWN misses here, not
    // as passes. If they rendered inside budget the sweep would fail as an
    // obsolete entry, and this test would be asserting the wrong thing.
    assert_eq!(
        sweep.known_misses.len(),
        BUDGET_TABLE.iter().filter(|r| r.known_miss.is_some()).count(),
        "fixture: every waived row must render as a known miss, got {:?}",
        sweep.known_misses
    );
    assert!(
        sweep.verdict().is_ok(),
        "every row at half its effective ceiling must pass: {:?}",
        sweep.verdict()
    );
}

/// A complete sweep still fails when a row misses, and the message names the
/// row — otherwise the gate is a bare "something regressed".
#[test]
fn a_complete_sweep_with_one_miss_fails_and_names_the_row() {
    let root = tempdir_for_test("budget_sweep_one_miss");
    populate_all_rows(&root, 0.5);
    // Blow out exactly one row. It must be a row with NO `known_miss`, or the
    // ceiling would absorb the overshoot and this would assert nothing —
    // `recall_hot_path`, the obvious choice and the one this test used to
    // pick, is currently a tracked miss.
    let victim = BUDGET_TABLE
        .iter()
        .find(|r| r.known_miss.is_none() && r.bench == "atomic_write")
        .expect("an unwaived row to blow out");
    write_synthetic_estimates(
        &root,
        victim.group,
        victim.bench,
        victim.label,
        victim.p50_budget_ns as f64 * 3.0,
    );

    let sweep = sweep_budgets(&root);
    assert_eq!(sweep.measured(), BUDGET_TABLE.len(), "fixture: every row still measured");
    assert_eq!(sweep.skipped.len(), 0, "fixture: nothing skipped");

    let err = sweep.verdict().expect_err("an over-budget row must fail the sweep");
    assert!(err.contains("atomic_write"), "message must name the offending bench: {err}");
    assert!(!err.contains("INCOMPLETE"), "this is a budget miss, not a coverage gap: {err}");
}

/// Synthetic-JSON unit test — verifies the parse + comparison logic without
/// requiring real Criterion data on disk. Always runs (no live-backend
/// dependency).
#[test]
fn asserts_present_estimates() {
    let tmp = tempdir_for_test("budget_assertions_present");
    write_synthetic_estimates(&tmp, "ingest_hot_path", "ingest_12kb_md", "moon", 30_000_000.0); // 30 ms median
    write_synthetic_sample(
        &tmp,
        "ingest_hot_path",
        "ingest_12kb_md",
        "moon",
        &[28_000_000.0, 29_000_000.0, 30_000_000.0, 31_000_000.0, 32_000_000.0],
    );

    let row = BudgetRow {
        group: "ingest_hot_path",
        bench: "ingest_12kb_md",
        label: "moon",
        p50_budget_ns: 50_000_000,
        p99_budget_ns: 110_000_000,
        known_miss: None,
    };

    let outcome = check_budget(&tmp, row).expect("check");
    let CheckOutcome::Reported(rep) = outcome else {
        panic!("expected Reported; got {outcome:?}");
    };
    assert!(rep.p50_within_budget, "30 ms should be within 50 ms budget");
    assert!(rep.p99_within_budget, "32 ms should be within 110 ms budget");
    assert!((rep.p50_ms - 30.0).abs() < 0.001);
}

/// Synthetic-JSON unit test for the OVER-budget path. Asserts the failure
/// message names the bench, the actual p50, the budget, and the % overshoot.
#[test]
fn asserts_over_budget_emits_actionable_message() {
    let tmp = tempdir_for_test("budget_assertions_over");
    write_synthetic_estimates(&tmp, "ingest_hot_path", "ingest_12kb_md", "moon", 73_000_000.0); // 73 ms — way over 50 ms budget
    write_synthetic_sample(
        &tmp,
        "ingest_hot_path",
        "ingest_12kb_md",
        "moon",
        &[70_000_000.0, 72_000_000.0, 73_000_000.0, 74_000_000.0, 75_000_000.0],
    );

    let row = BudgetRow {
        group: "ingest_hot_path",
        bench: "ingest_12kb_md",
        label: "moon",
        p50_budget_ns: 50_000_000,
        p99_budget_ns: 110_000_000,
        known_miss: None,
    };

    let outcome = check_budget(&tmp, row).expect("check");
    let CheckOutcome::Reported(rep) = outcome else {
        panic!("expected Reported; got {outcome:?}");
    };
    assert!(!rep.p50_within_budget);
    let msg = format_failure(&rep);
    assert!(msg.contains("p50"), "msg missing 'p50': {msg}");
    assert!(msg.contains("73"), "msg missing actual ms: {msg}");
    assert!(msg.contains("50"), "msg missing budget ms: {msg}");
    assert!(msg.contains('%'), "msg missing percentage overshoot: {msg}");
}

/// Synthetic-JSON unit test for the missing-data path. Asserts that a
/// missing JSON file → Skipped(_), NOT a panic — Plan's "skip when no
/// baseline data" requirement.
#[test]
fn missing_estimates_skip_without_panic() {
    let tmp = tempdir_for_test("budget_assertions_missing");
    let row = BudgetRow {
        group: "no_such_group",
        bench: "no_such_bench",
        label: "moon",
        p50_budget_ns: 1,
        p99_budget_ns: 1,
        known_miss: None,
    };
    let outcome = check_budget(&tmp, row).expect("check");
    assert!(matches!(outcome, CheckOutcome::Skipped(_)), "missing data must skip; got {outcome:?}");
}

// ----- check_budget core -----

fn check_budget(target_root: &Path, row: BudgetRow) -> Result<CheckOutcome, String> {
    // Criterion writes per-bench data to:
    //   target/criterion/<group>/<bench>_<label>/new/estimates.json
    //   target/criterion/<group>/<bench>_<label>/new/sample.json
    // The bench id Criterion derives from `BenchmarkId::new("recall_q",
    // "moon")` is `recall_q/moon` so the on-disk dir is exactly that.
    let bench_dir =
        target_root.join(row.group).join(format!("{}/{}", row.bench, row.label)).join("new");
    let estimates_path = bench_dir.join("estimates.json");
    let sample_path = bench_dir.join("sample.json");

    if !estimates_path.exists() {
        return Ok(CheckOutcome::Skipped(format!(
            "no estimates.json at {} — run `cargo bench -p lunaris-bench --bench {}` first",
            estimates_path.display(),
            row.group
        )));
    }

    let p50_ns = parse_median_ns(&estimates_path).map_err(|e| format!("parse estimates: {e}"))?;
    let p99_ns = if sample_path.exists() {
        match parse_p99_ns(&sample_path) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, path = %sample_path.display(), "sample.json parse failed; treating p99 as same as p50");
                p50_ns
            }
        }
    } else {
        // No sample → use median as p99 fallback (best-effort; logged above).
        p50_ns
    };

    let p50_within_budget = p50_ns <= row.p50_budget_ns as f64;
    let p99_within_budget = if row.p99_budget_ns == 0 {
        true // no p99 budget specified for this row
    } else {
        p99_ns <= row.p99_budget_ns as f64
    };

    Ok(CheckOutcome::Reported(BudgetReport {
        group: row.group.to_string(),
        bench: row.bench.to_string(),
        label: row.label.to_string(),
        p50_ms: p50_ns / 1_000_000.0,
        p99_ms: p99_ns / 1_000_000.0,
        p50_budget_ms: row.p50_budget_ns as f64 / 1_000_000.0,
        p99_budget_ms: row.p99_budget_ns as f64 / 1_000_000.0,
        p50_within_budget,
        p99_within_budget,
    }))
}

// ----- JSON parse helpers -----

fn parse_median_ns(path: &Path) -> Result<f64, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("decode JSON: {e}"))?;
    v.get("median")
        .and_then(|m| m.get("point_estimate"))
        .and_then(|pe| pe.as_f64())
        .ok_or_else(|| format!("missing .median.point_estimate in {}", path.display()))
}

/// Compute the 99th percentile from Criterion's `sample.json`. The file is
/// either a JSON object containing a `times` array (Criterion 0.5+) or an
/// array of `[iter_count, total_time_ns]` tuples (older formats). We accept
/// both — the unit tests below cover the object-with-times shape.
fn parse_p99_ns(path: &Path) -> Result<f64, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("decode JSON: {e}"))?;

    // Try `{"times": [...], "iters": [...]}`
    if let (Some(times), Some(iters)) =
        (v.get("times").and_then(|x| x.as_array()), v.get("iters").and_then(|x| x.as_array()))
        && !times.is_empty()
        && times.len() == iters.len()
    {
        let mut per_iter: Vec<f64> = times
            .iter()
            .zip(iters.iter())
            .filter_map(|(t, n)| {
                let total_ns = t.as_f64()?;
                let iter_n = n.as_f64()?;
                if iter_n > 0.0 { Some(total_ns / iter_n) } else { None }
            })
            .collect();
        return percentile(&mut per_iter, 0.99);
    }

    // Fallback: `[[iter, total_ns], …]` shape.
    if let Some(rows) = v.as_array()
        && !rows.is_empty()
    {
        let mut per_iter: Vec<f64> = rows
            .iter()
            .filter_map(|r| {
                let arr = r.as_array()?;
                if arr.len() != 2 {
                    return None;
                }
                let iter_n = arr[0].as_f64()?;
                let total_ns = arr[1].as_f64()?;
                if iter_n > 0.0 { Some(total_ns / iter_n) } else { None }
            })
            .collect();
        return percentile(&mut per_iter, 0.99);
    }

    Err(format!("sample.json shape not recognised at {}", path.display()))
}

fn percentile(values: &mut [f64], q: f64) -> Result<f64, String> {
    if values.is_empty() {
        return Err("empty samples".into());
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Index of the q-th percentile per the simplest "nearest-rank" rule.
    // For q=0.99 and n=100 → index 98 (sorted[98] == 99th percentile).
    let idx = ((values.len() as f64 - 1.0) * q).round() as usize;
    Ok(values[idx])
}

// ----- formatting helpers -----

fn format_failure(rep: &BudgetReport) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !rep.p50_within_budget {
        let overshoot = (rep.p50_ms / rep.p50_budget_ms - 1.0) * 100.0;
        parts.push(format!(
            "p50 {:.2} ms > {:.2} ms budget — over by +{:.0}%",
            rep.p50_ms, rep.p50_budget_ms, overshoot
        ));
    }
    if !rep.p99_within_budget {
        let overshoot = (rep.p99_ms / rep.p99_budget_ms - 1.0) * 100.0;
        parts.push(format!(
            "p99 {:.2} ms > {:.2} ms budget — over by +{:.0}%",
            rep.p99_ms, rep.p99_budget_ms, overshoot
        ));
    }
    parts.join("; ")
}

fn print_report(sweep: &Sweep) {
    eprintln!("\n=== Moon latency budget report (blueprint §4.1/§4.2) ===");
    eprintln!("Rows in table: {}", BUDGET_TABLE.len());
    eprintln!("Measured:      {}", sweep.measured());
    eprintln!("Passed:        {}", sweep.passed.len());
    eprintln!("Skipped:       {}", sweep.skipped.len());
    eprintln!("Known misses:  {}", sweep.known_misses.len());
    eprintln!("Hard failures: {}", sweep.hard_failures.len());
    eprintln!();
    for p in &sweep.passed {
        eprintln!("  PASS   {p}");
    }
    for s in &sweep.skipped {
        eprintln!("  SKIP   {s}");
    }
    for k in &sweep.known_misses {
        eprintln!("  KNOWN  {k}");
    }
    for f in &sweep.hard_failures {
        eprintln!("  HARD   {f}");
    }
    eprintln!();
}

// ----- env / fs helpers -----

fn criterion_root() -> PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    PathBuf::from(target).join("criterion")
}

fn tempdir_for_test(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lunaris-bench-{name}-{}", std::process::id()));
    if d.exists() {
        let _ = fs::remove_dir_all(&d);
    }
    fs::create_dir_all(&d).expect("create tempdir");
    d
}

fn write_synthetic_estimates(root: &Path, group: &str, bench: &str, label: &str, median_ns: f64) {
    let dir = root.join(group).join(format!("{bench}/{label}")).join("new");
    fs::create_dir_all(&dir).expect("create estimates dir");
    let body = serde_json::json!({
        "median": { "point_estimate": median_ns, "confidence_interval": { "lower_bound": median_ns * 0.95, "upper_bound": median_ns * 1.05 }, "standard_error": median_ns * 0.01 },
        "mean":   { "point_estimate": median_ns, "confidence_interval": { "lower_bound": median_ns * 0.95, "upper_bound": median_ns * 1.05 }, "standard_error": median_ns * 0.01 },
    });
    fs::write(dir.join("estimates.json"), serde_json::to_vec_pretty(&body).unwrap())
        .expect("write estimates.json");
}

fn write_synthetic_sample(root: &Path, group: &str, bench: &str, label: &str, per_iter_ns: &[f64]) {
    let dir = root.join(group).join(format!("{bench}/{label}")).join("new");
    fs::create_dir_all(&dir).expect("create sample dir");
    let times: Vec<f64> = per_iter_ns.to_vec();
    let iters: Vec<f64> = (0..per_iter_ns.len()).map(|_| 1.0).collect();
    let body = serde_json::json!({
        "times": times,
        "iters": iters,
    });
    fs::write(dir.join("sample.json"), serde_json::to_vec_pretty(&body).unwrap())
        .expect("write sample.json");
}

fn init_tracing() {
    // Best-effort init — `try_init` is OK if another test already installed
    // the global subscriber.
    let _ = tracing_subscriber::fmt::try_init();
}

// Bring tracing_subscriber into scope only inside this test file (it isn't
// declared as a workspace dep — pull from the lunaris-core dep tree where
// it's already present).
mod tracing_subscriber {
    pub mod fmt {
        pub fn try_init() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
            // No-op: avoid pulling tracing-subscriber as a dep here. The
            // test's tracing::warn! lines still print to stderr via the
            // default no-op subscriber; this fn exists so the test code
            // shape stays clean.
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Known-miss ratchet
// ---------------------------------------------------------------------------

/// Build a synthetic report at an explicit p50/p99, so the four outcomes can
/// be driven without Criterion data on disk.
fn report_at(row: &BudgetRow, p50_ns: f64, p99_ns: f64) -> BudgetReport {
    BudgetReport {
        group: row.group.to_string(),
        bench: row.bench.to_string(),
        label: row.label.to_string(),
        p50_ms: p50_ns / 1e6,
        p99_ms: p99_ns / 1e6,
        p50_budget_ms: row.p50_budget_ns as f64 / 1e6,
        p99_budget_ms: row.p99_budget_ns as f64 / 1e6,
        p50_within_budget: p50_ns <= row.p50_budget_ns as f64,
        p99_within_budget: row.p99_budget_ns == 0 || p99_ns <= row.p99_budget_ns as f64,
    }
}

fn waived_row() -> BudgetRow {
    BudgetRow {
        group: "ingest_hot_path",
        bench: "synthetic_waived",
        label: "moon",
        p50_budget_ns: 300_000_000,
        p99_budget_ns: 570_000_000,
        known_miss: Some(KnownMiss {
            tracked_as: "pilotspace/moon#719",
            p50_ceiling_ns: 1_200_000_000,
            p99_ceiling_ns: 2_000_000_000,
            why: "synthetic fixture",
        }),
    }
}

/// A tracked miss inside its ceiling is reported, not failed. Without this the
/// only way to keep CI green over a diagnosed upstream defect is to delete the
/// row or loosen its budget, and both of those lose the contract permanently.
#[test]
fn a_tracked_miss_inside_its_ceiling_is_not_a_failure() {
    let row = waived_row();
    // 863 ms — the measured INGEST-06 p50, over the 300 ms budget and under
    // the 1200 ms ceiling.
    let rep = report_at(&row, 863_000_000.0, 1_328_000_000.0);
    assert!(!rep.p50_within_budget, "fixture: this must be over budget");
    assert_eq!(classify_row(&row, &rep), RowVerdict::KnownMiss);
}

/// The ceiling is the load-bearing half. An exception that absorbed any
/// regression would turn the row off, which is indistinguishable from
/// deleting it.
#[test]
fn a_tracked_miss_past_its_ceiling_still_fails() {
    let row = waived_row();
    let rep = report_at(&row, 1_500_000_000.0, 1_328_000_000.0);
    assert_eq!(classify_row(&row, &rep), RowVerdict::Fail);
}

/// A p99 blow-out must not hide behind a p50 that is still under its ceiling.
#[test]
fn a_tracked_miss_fails_on_p99_ceiling_too() {
    let row = waived_row();
    let rep = report_at(&row, 863_000_000.0, 2_500_000_000.0);
    assert_eq!(classify_row(&row, &rep), RowVerdict::Fail);
}

/// The reverse edge. When the cause is fixed the row comes back inside
/// budget, and nothing would ever say so — the exception would sit there
/// forever, well documented and unread. So this is a failure with a to-do.
#[test]
fn a_tracked_miss_that_meets_budget_fails_so_the_entry_gets_deleted() {
    let row = waived_row();
    let rep = report_at(&row, 120_000_000.0, 200_000_000.0);
    assert!(rep.p50_within_budget, "fixture: this must be inside budget");
    assert_eq!(classify_row(&row, &rep), RowVerdict::ObsoleteWaiver);
}

/// An unwaived row is unaffected by any of the above.
#[test]
fn an_unwaived_row_still_passes_and_fails_on_its_budget_alone() {
    let mut row = waived_row();
    row.known_miss = None;
    let over = report_at(&row, 863_000_000.0, 1_328_000_000.0);
    assert_eq!(classify_row(&row, &over), RowVerdict::Fail);
    let under = report_at(&row, 120_000_000.0, 200_000_000.0);
    assert_eq!(classify_row(&row, &under), RowVerdict::Pass);
}

/// Every entry in the shipped table must name where its cause is tracked and
/// why. An exception with no reference is one nobody can ever retire.
#[test]
fn every_known_miss_in_the_table_names_a_tracking_reference() {
    for row in BUDGET_TABLE {
        let Some(km) = row.known_miss else { continue };
        let id = format!("{}/{}/{}", row.group, row.bench, row.label);
        assert!(!km.tracked_as.trim().is_empty(), "{id}: known miss with no tracking reference");
        assert!(km.why.len() > 20, "{id}: known miss with no usable reason: {:?}", km.why);
        assert!(
            km.p50_ceiling_ns > row.p50_budget_ns,
            "{id}: ceiling {} is not above the budget {} — the entry could never be anything but \
             obsolete",
            km.p50_ceiling_ns,
            row.p50_budget_ns
        );
    }
}
