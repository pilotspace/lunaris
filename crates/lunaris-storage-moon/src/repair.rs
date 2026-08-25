//! Repair for rows written before the F22 write-side guard.
//!
//! `atomic.rs` refuses to write a `vec` field for an embedding that is all-zero
//! or non-finite, so no new row can reach the KNN index unindexable. That is a
//! forward-only fix: a store that was running before it landed still holds the
//! rows written under the old behaviour, and nothing in the system removes
//! them. `lunaris-hook`'s embed-promotion worker is driven by
//! `publish_capture_receipt` and only ever sees rows an event was published
//! for; it never scans, so a legacy row has no event and never acquires one.
//!
//! A zero vector is not merely unmatched — it is matched *equally* by
//! everything. Moon's chunk index is HNSW/COSINE and a direction-less vector
//! sits at distance 1.0 from every query, which under the `1/(1+d)` score
//! Lunaris reports is a flat 0.500. That beats every genuine hit below cosine
//! 0.5, on every query, with no relationship to the text — so the row is not
//! just wrong, it is wrong in a way no one can diagnose from the content.
//!
//! [`MoonStorage::repair_unindexable_vectors`] brings those rows into the shape
//! the guard would have written: it drops the `vec` field and leaves the row
//! otherwise intact. `meta` and `content` stay, so hydration and BM25 continue
//! to see the document and a later re-embed has somewhere to write the real
//! vector back to. Deleting the row would throw the document away to fix an
//! index entry.

use std::collections::HashSet;

use lunaris_core::{Scope, StorageError};

use crate::MoonStorage;
use crate::atomic::unindexable_reason;
use crate::client::moon_err;
use crate::keyspace::ft_index_name;

/// How many keys to ask Moon for per `SCAN` round-trip. Large enough that a
/// million-row scope is not a million round-trips, small enough that a single
/// reply stays well inside a normal socket buffer.
const SCAN_BATCH: usize = 512;

/// What a repair pass found, and what it did about it.
///
/// `unindexable` is the count of damaged rows the pass *identified* and
/// `repaired` the count it *changed*; they are equal after a real run and
/// differ deliberately after a dry run, which is the whole point of having
/// both. Reporting a single number would make "found nothing" and "fixed
/// nothing" indistinguishable in an operator's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VectorRepairReport {
    /// Rows walked in this scope and index, damaged or not.
    pub scanned: usize,
    /// Rows carrying a `vec` field that cannot be a KNN candidate.
    pub unindexable: usize,
    /// Rows whose `vec` field was actually removed. Always 0 for a dry run.
    pub repaired: usize,
    /// Whether this pass was permitted to write.
    pub dry_run: bool,
}

/// Decodes a stored `vec` field and reports why it cannot be indexed, if so.
///
/// Moon stores the embedding as little-endian `f32`s (see `atomic.rs`), so a
/// length that is not a multiple of four is not an embedding at all — it is
/// counted as damage rather than skipped, because leaving it in place would
/// leave a row in the index that no query can explain.
fn stored_vec_is_unindexable(raw: &[u8]) -> Option<&'static str> {
    if raw.is_empty() {
        return Some("empty");
    }
    if !raw.len().is_multiple_of(4) {
        return Some("malformed-length");
    }
    let decoded: Vec<f32> =
        raw.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    unindexable_reason(&decoded)
}

impl MoonStorage {
    /// Removes the `vec` field from every row in `scope`/`index` whose stored
    /// embedding cannot be a KNN candidate, leaving the rest of the row alone.
    ///
    /// Pass `dry_run = true` to count the damage without touching the store —
    /// an operator pointing this at a production scope is owed the number
    /// before the mutation.
    ///
    /// The scan is bounded by the per-scope FT index name, so a repair of one
    /// tenant cannot read or write another tenant's rows.
    pub async fn repair_unindexable_vectors(
        &self,
        scope: &Scope,
        index: &str,
        dry_run: bool,
    ) -> Result<VectorRepairReport, StorageError> {
        let mut typed = self.client().typed();
        let pattern = format!("{}:*", ft_index_name(scope, index));
        let mut report = VectorRepairReport { dry_run, ..Default::default() };
        let mut cursor = 0u64;
        // SCAN guarantees every key present for the whole sweep is returned at
        // least ONCE, not exactly once — a rehash mid-sweep can hand the same
        // key back twice. Deduplicating matters because `scanned` and
        // `unindexable` are the census this repair exists to produce, and a
        // count that drifts upward under load is the kind of quietly-wrong
        // number nobody can distinguish from real damage.
        let mut seen: HashSet<Vec<u8>> = HashSet::new();

        loop {
            let (next, keys): (u64, Vec<Vec<u8>>) =
                typed.scan_match(pattern.as_bytes(), SCAN_BATCH, cursor).await.map_err(moon_err)?;

            for key in keys {
                if !seen.insert(key.clone()) {
                    continue;
                }
                report.scanned += 1;
                let raw: Option<Vec<u8>> =
                    typed.hget(key.as_slice(), "vec").await.map_err(moon_err)?;
                // No `vec` at all is the post-guard shape, and also the shape
                // this pass leaves behind — so a second run is a no-op rather
                // than an error, which is what makes the repair safe to retry.
                let Some(raw) = raw else { continue };
                let Some(reason) = stored_vec_is_unindexable(&raw) else { continue };
                report.unindexable += 1;
                tracing::info!(
                    target: "lunaris::storage::moon::repair",
                    scope = %scope.as_str(),
                    index = %index,
                    reason,
                    dry_run,
                    "row carries a `vec` that cannot be a KNN candidate (F22)"
                );
                if !dry_run {
                    let _: i64 = typed.hdel(key.as_slice(), "vec").await.map_err(moon_err)?;
                    report.repaired += 1;
                }
            }

            if next == 0 {
                break;
            }
            cursor = next;
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    #[test]
    fn a_real_embedding_is_left_alone() {
        assert_eq!(stored_vec_is_unindexable(&encode(&[0.0, 1.0, 0.0])), None);
        // A single non-zero component is enough of a direction to match on.
        assert_eq!(stored_vec_is_unindexable(&encode(&[0.0, 0.0, 1e-9])), None);
    }

    #[test]
    fn an_all_zero_embedding_is_damage() {
        assert_eq!(stored_vec_is_unindexable(&encode(&[0.0; 768])), Some("all-zero"));
    }

    #[test]
    fn a_non_finite_embedding_is_damage() {
        assert_eq!(stored_vec_is_unindexable(&encode(&[1.0, f32::NAN])), Some("non-finite"));
        assert_eq!(stored_vec_is_unindexable(&encode(&[1.0, f32::INFINITY])), Some("non-finite"));
    }

    #[test]
    fn bytes_that_are_not_an_f32_vector_are_damage() {
        // Not a multiple of four: whatever this row is, it is not an embedding,
        // and it must not be left sitting in the index unexplained.
        assert_eq!(stored_vec_is_unindexable(&[0u8; 7]), Some("malformed-length"));
        assert_eq!(stored_vec_is_unindexable(&[]), Some("empty"));
    }

    #[test]
    fn a_dry_run_report_distinguishes_found_from_fixed() {
        let dry = VectorRepairReport { scanned: 9, unindexable: 4, repaired: 0, dry_run: true };
        let wet = VectorRepairReport { scanned: 9, unindexable: 4, repaired: 4, dry_run: false };
        assert_ne!(dry, wet, "the two must not be collapsible into the same report");
    }
}
