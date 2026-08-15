//! Post-migration verification: per-kind counts on both sides, plus a
//! byte-for-byte content sample.
//!
//! Verification re-reads through the same `StoragePort` surface the copy used,
//! so it proves what an application will actually see — not what the writer
//! believed it wrote.

#[allow(unused_imports)]
use std::collections::BTreeMap;

#[allow(unused_imports)]
use futures::StreamExt;
use lunaris_core::keyspace::scope_prefix;
use lunaris_core::{Scope, StoragePort};

use crate::migrate::MigrateError;
use crate::plan::{RowVerdict, classify_row, kind_of};

/// How many example keys a report keeps per failure class. Counts are exact;
/// the examples exist so an operator has something to grep for.
pub const MAX_EXAMPLES: usize = 20;

/// Outcome of one scope's verification pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Scope verified.
    pub scope: String,
    /// Rows on the source that SHOULD be present on the destination.
    pub source_eligible: u64,
    /// Rows found under the scope prefix on the destination.
    pub dest_rows: u64,
    /// Eligible source rows per kind.
    pub by_kind_source: BTreeMap<String, u64>,
    /// Destination rows per kind.
    pub by_kind_dest: BTreeMap<String, u64>,
    /// Eligible source keys absent from the destination.
    pub missing: u64,
    /// Keys present on both sides whose bytes differ.
    pub mismatched: u64,
    /// Keys content-compared (the rest are presence-checked only).
    pub sampled: u64,
    /// Destination keys with no eligible source counterpart. Informational, not
    /// a failure: a re-run onto a live store, or a skipped closed-interval row
    /// that a previous tool version copied, both land here.
    pub dest_only: u64,
    /// Up to [`MAX_EXAMPLES`] missing keys.
    pub missing_examples: Vec<String>,
    /// Up to [`MAX_EXAMPLES`] mismatched keys.
    pub mismatch_examples: Vec<String>,
}

impl VerifyReport {
    /// A pass: every eligible source row is on the destination, and every
    /// sampled row is byte-identical.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.missing == 0 && self.mismatched == 0
    }
}

/// Compare one scope across `source` and `dest`.
///
/// `sample` caps the number of keys read back for a full byte comparison; the
/// sample is spread deterministically across the key order rather than taken
/// from the head, so a truncated write is caught as readily as a corrupt one.
///
/// Both sides are buffered in memory — a migration tool runs once, on an
/// operator's machine, over one scope at a time. `scan_range` on both backends
/// already buffers the full prefix internally, so this adds no new bound.
pub async fn verify_scope(
    source: &dyn StoragePort,
    dest: &dyn StoragePort,
    scope: &Scope,
    sample: usize,
) -> Result<VerifyReport, MigrateError> {
    // RED: scaffolding only — the comparison itself lands in the GREEN commit.
    let _ = (source, dest, sample, push_example, classify_row, kind_of, RowVerdict::Migrate);
    let _ = scope_prefix(scope);
    Ok(VerifyReport { scope: scope.as_str().to_owned(), ..VerifyReport::default() })
}

fn push_example(bucket: &mut Vec<String>, key: &[u8]) {
    if bucket.len() < MAX_EXAMPLES {
        bucket.push(String::from_utf8_lossy(key).into_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_report_is_ok() {
        let r = VerifyReport { source_eligible: 10, dest_rows: 10, ..VerifyReport::default() };
        assert!(r.ok());
    }

    #[test]
    fn missing_or_mismatched_fails_the_pass() {
        assert!(!VerifyReport { missing: 1, ..VerifyReport::default() }.ok());
        assert!(!VerifyReport { mismatched: 1, ..VerifyReport::default() }.ok());
    }

    #[test]
    fn dest_only_rows_do_not_fail_the_pass() {
        // A destination that already held data is the idempotent-re-run case.
        assert!(VerifyReport { dest_only: 7, ..VerifyReport::default() }.ok());
    }

    #[test]
    fn examples_are_capped() {
        let mut bucket = Vec::new();
        for i in 0..(MAX_EXAMPLES + 10) {
            push_example(&mut bucket, format!("k{i}").as_bytes());
        }
        assert_eq!(bucket.len(), MAX_EXAMPLES);
    }
}
