//! Plan 13-02 — EVAL-05 deterministic 10K-turn Helios chat trace generator.
//!
//! Produces a seeded, reproducible sequence of [`Op`] values that matches the
//! helios-rfc §5.3 Read/Write/Edit/Grep telemetry mix. Reuses the same seed as
//! Plan 12-03's `helios_p50` Criterion bench (`0xBEEF_DEAD`) so the resulting
//! recall p50 is apples-to-apples comparable with the micro-bench baseline.
//!
//! The binary `eval_05_helios_10k` consumes this module; unit tests validate
//! determinism, mix distribution, path-pool stability, and compact JSON size.
//!
//! ## Mix (verbatim from Plan 12-03; do not drift)
//!
//! ```text
//! read   0.40
//! write  0.30
//! edit   0.15
//! grep   0.15
//! ```
//!
//! ## Path pool
//!
//! All ops reference one of 500 synthetic paths of the form
//! `helios:fs/chat-turn-NNNN.md`. The binary's warm-up MUST seed every path in
//! the pool before replay so read/edit/grep ops hit existing keys (measurement
//! hygiene — empty reads are artificially fast and would bias the p50).

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha12Rng;
use serde::{Deserialize, Serialize};

/// Deterministic seed — same as Plan 12-03 `helios_p50` bench.
/// Hex `0xBEEF_DEAD` == decimal `3_204_447_181`.
pub const EVAL_05_SEED: u64 = 0xBEEF_DEAD;

/// Trace length — 10K turns per RELEASE-03 acceptance.
pub const EVAL_05_LEN: usize = 10_000;

/// Size of the shared path pool. All reads/writes/edits/greps sample from here
/// so reads hit real content once warm-up has populated every pool entry.
pub const EVAL_05_POOL_SIZE: usize = 500;

/// Mix weights — MUST sum to 1.0.
pub const MIX_READ: f64 = 0.40;
pub const MIX_WRITE: f64 = 0.30;
pub const MIX_EDIT: f64 = 0.15;
pub const MIX_GREP: f64 = 0.15;

/// A single Helios turn operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Op {
    Read {
        path: String,
    },
    Write {
        path: String,
        content: String,
    },
    Edit {
        path: String,
        find: String,
        replace: String,
    },
    Grep {
        pattern: String,
    },
}

/// Deterministic 10K-turn Helios chat trace.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trace {
    pub seed: u64,
    pub ops: Vec<Op>,
}

impl Trace {
    /// Build a fresh trace of `len` ops seeded from `seed`. The resulting
    /// `Vec<Op>` is byte-identical across invocations for a fixed `(seed, len)`
    /// pair (ChaCha12 is portable + deterministic).
    pub fn new(seed: u64, len: usize) -> Self {
        let pool = path_pool();
        let mut rng = ChaCha12Rng::seed_from_u64(seed);
        let mut ops = Vec::with_capacity(len);
        for i in 0..len {
            let idx = rng.random_range(0..pool.len());
            let path = pool[idx].clone();
            let r: f64 = rng.random();
            let op = if r < MIX_READ {
                Op::Read { path }
            } else if r < MIX_READ + MIX_WRITE {
                Op::Write {
                    path,
                    content: format!("turn {i} body lorem ipsum dolor sit amet"),
                }
            } else if r < MIX_READ + MIX_WRITE + MIX_EDIT {
                Op::Edit {
                    path,
                    find: "lorem".into(),
                    replace: "LOREM".into(),
                }
            } else {
                // Grep patterns reference pool indices so the search space is
                // bounded + deterministic.
                let grep_idx = rng.random_range(0..pool.len());
                Op::Grep {
                    pattern: format!("turn {grep_idx}"),
                }
            };
            ops.push(op);
        }
        Self { seed, ops }
    }

    /// The canonical path pool (500 stable synthetic paths). Exposed so the
    /// binary's warm-up phase can seed all of them before trace replay.
    pub fn path_pool(&self) -> Vec<String> {
        path_pool()
    }
}

fn path_pool() -> Vec<String> {
    (0..EVAL_05_POOL_SIZE)
        .map(|i| format!("helios:fs/chat-turn-{i:04}.md"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_is_deterministic() {
        let a = Trace::new(EVAL_05_SEED, EVAL_05_LEN);
        let b = Trace::new(EVAL_05_SEED, EVAL_05_LEN);
        assert_eq!(a.ops, b.ops, "same (seed, len) must produce identical ops");
        assert_eq!(a.seed, EVAL_05_SEED);
        assert_eq!(a.ops.len(), EVAL_05_LEN);
    }

    #[test]
    fn mix_within_1pct_of_weights() {
        let t = Trace::new(EVAL_05_SEED, EVAL_05_LEN);
        let mut reads = 0usize;
        let mut writes = 0usize;
        let mut edits = 0usize;
        let mut greps = 0usize;
        for op in &t.ops {
            match op {
                Op::Read { .. } => reads += 1,
                Op::Write { .. } => writes += 1,
                Op::Edit { .. } => edits += 1,
                Op::Grep { .. } => greps += 1,
            }
        }

        let total = t.ops.len() as f64;
        let tol = 0.01 * total; // ±1% of 10K = ±100
        let within = |actual: usize, weight: f64| {
            let expected = weight * total;
            ((actual as f64) - expected).abs() <= tol
        };
        assert!(
            within(reads, MIX_READ),
            "reads={reads} not within ±1% of {}",
            MIX_READ * total
        );
        assert!(
            within(writes, MIX_WRITE),
            "writes={writes} not within ±1% of {}",
            MIX_WRITE * total
        );
        assert!(
            within(edits, MIX_EDIT),
            "edits={edits} not within ±1% of {}",
            MIX_EDIT * total
        );
        assert!(
            within(greps, MIX_GREP),
            "greps={greps} not within ±1% of {}",
            MIX_GREP * total
        );
    }

    #[test]
    fn paths_are_stable_pool() {
        let t = Trace::new(EVAL_05_SEED, EVAL_05_LEN);
        let pool: std::collections::HashSet<String> = t.path_pool().into_iter().collect();
        assert_eq!(pool.len(), EVAL_05_POOL_SIZE);
        for op in &t.ops {
            let maybe_path = match op {
                Op::Read { path } => Some(path),
                Op::Write { path, .. } => Some(path),
                Op::Edit { path, .. } => Some(path),
                Op::Grep { .. } => None,
            };
            if let Some(p) = maybe_path {
                assert!(
                    pool.contains(p),
                    "op path {p} not drawn from the canonical 500-path pool"
                );
            }
        }
    }

    #[test]
    fn trace_serializes_compactly() {
        let t = Trace::new(EVAL_05_SEED, EVAL_05_LEN);
        let json = serde_json::to_string(&t).expect("serialize trace");
        assert!(
            json.len() < 1_000_000,
            "10K-op trace JSON exceeded 1 MB envelope: len={}",
            json.len()
        );
    }

    #[test]
    fn mix_weights_sum_to_one() {
        let sum = MIX_READ + MIX_WRITE + MIX_EDIT + MIX_GREP;
        assert!((sum - 1.0).abs() < 1e-9, "mix weights must sum to 1.0; got {sum}");
    }
}
