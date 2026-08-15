//! The copy engine: read every current primitive out of the source through
//! `StoragePort`, classify it, and re-write the survivors into the destination
//! as idempotent `KvPut` batches.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use futures::StreamExt;
use lunaris_core::keyspace::scope_prefix;
use lunaris_core::storage::WriteOp;
use lunaris_core::{Scope, StoragePort};

use crate::plan::{MigrationOptions, RowVerdict, classify_row, kind_of, needs_reembed};

/// Failure modes of a migration run.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// `--commit` without `--acknowledge-lossy`.
    #[error("{}", crate::contract::ACK_REQUIRED)]
    LossyNotAcknowledged,
    /// A source read or destination write failed.
    #[error("storage: {0}")]
    Storage(#[from] lunaris_core::StorageError),
    /// The re-embed manifest could not be written.
    #[error("manifest: {0}")]
    Manifest(#[from] std::io::Error),
}

/// What one scope's migration did — and, just as importantly, did not do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeReport {
    /// The scope this report covers.
    pub scope: String,
    /// Rows returned by the source scan (every key under `lunaris:{scope}:`).
    pub scanned: u64,
    /// Rows eligible to migrate.
    pub eligible: u64,
    /// Rows actually written to the destination. Always `0` on a dry run —
    /// this is the field the dry-run guarantee is asserted on.
    pub written: u64,
    /// Eligible rows per primitive kind, e.g. `{"episode": 12, "fact": 40}`.
    pub by_kind: BTreeMap<String, u64>,
    /// Skipped: record validity closed (`bt.valid.1` set) — retracted state.
    pub skipped_closed_valid: u64,
    /// Skipped: record sys interval closed (`bt.sys.1` set) — logically deleted.
    pub skipped_closed_sys: u64,
    /// Skipped: key outside the canonical keyspace or belonging to another scope.
    pub skipped_foreign_key: u64,
    /// Eligible rows whose kind carries an embedding, i.e. the size of the
    /// re-embed backlog this migration creates.
    pub needs_reembed: u64,
    /// `atomic_write` calls issued (`0` on a dry run).
    pub batches: u64,
}

impl ScopeReport {
    /// Total rows deliberately left behind.
    #[must_use]
    pub fn skipped(&self) -> u64 {
        self.skipped_closed_valid + self.skipped_closed_sys + self.skipped_foreign_key
    }
}

/// Enumerate the scopes present in `source`.
///
/// Returns `Err(StorageError::NotSupported)` from backends without scope
/// enumeration — Postgres is the documented case (its RLS boundary makes a
/// cross-scope `SELECT DISTINCT scope` impossible under the app role), so a
/// Postgres source must be migrated with explicit `--scope` arguments.
/// Whether the backend failed to advance pagination: it echoed the cursor it
/// was handed, or handed back an empty one. Either way another round trip would
/// return the same page forever.
fn pagination_stalled(prev: Option<&str>, next: &str) -> bool {
    next.is_empty() || prev == Some(next)
}

pub async fn discover_scopes(source: &dyn StoragePort) -> Result<Vec<Scope>, MigrateError> {
    const PAGE: usize = 500;
    let mut out: Vec<Scope> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = source.list_scopes(None, PAGE, cursor.as_deref()).await?;
        out.extend(page.scopes);
        match page.next_cursor {
            Some(next) => {
                if pagination_stalled(cursor.as_deref(), &next) {
                    // Stop with what we have rather than spin. The scopes
                    // collected so far are still valid, and `--scope` is the
                    // documented fallback for an enumeration this tool cannot
                    // trust.
                    tracing::warn!(
                        scopes_so_far = out.len(),
                        "list_scopes did not advance its cursor — stopping enumeration; \
                         pass explicit --scope arguments if a scope is missing"
                    );
                    break;
                }
                cursor = Some(next);
            }
            None => break,
        }
    }
    out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    out.dedup_by(|a, b| a.as_str() == b.as_str());
    Ok(out)
}

/// Migrate one scope from `source` into `dest`.
///
/// Reads the whole `lunaris:{scope}:` prefix in one `scan_range` (the current
/// row per key — a historical `as_of` is refused by the destination anyway),
/// classifies each row, and writes the survivors in `batch_size` `KvPut`
/// batches. Writes happen only when [`MigrationOptions::writes_enabled`].
pub async fn migrate_scope(
    source: &dyn StoragePort,
    dest: &dyn StoragePort,
    scope: &Scope,
    opts: &MigrationOptions,
) -> Result<ScopeReport, MigrateError> {
    if opts.commit && !opts.acknowledge_lossy {
        return Err(MigrateError::LossyNotAcknowledged);
    }
    let mut report = ScopeReport { scope: scope.as_str().to_owned(), ..ScopeReport::default() };
    let mut manifest = match &opts.reembed_manifest {
        Some(path) => Some(open_manifest(path)?),
        None => None,
    };

    let prefix = scope_prefix(scope).into_bytes();
    let mut batch: Vec<WriteOp> = Vec::with_capacity(opts.batch_size);
    // Buffered up front so the source stream is closed before the first write:
    // the two handles are never live at the same time, which matters on a
    // single-connection backend and keeps a mid-run failure from leaving a
    // half-drained stream behind.
    let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut stream = source.scan_range(scope, &prefix, None).await?;
    while let Some(row) = stream.next().await {
        let (key, value) = row?;
        rows.push((key.to_vec(), value.to_vec()));
    }
    drop(stream);

    for (key, value) in rows {
        report.scanned += 1;
        match classify_row(scope, &key, &value) {
            RowVerdict::SkipClosedValid => {
                report.skipped_closed_valid += 1;
                continue;
            }
            RowVerdict::SkipClosedSys => {
                report.skipped_closed_sys += 1;
                continue;
            }
            RowVerdict::SkipForeignKey => {
                report.skipped_foreign_key += 1;
                continue;
            }
            RowVerdict::Migrate => {}
        }
        report.eligible += 1;
        let kind = kind_of(&key).map(|(_, k)| k).unwrap_or("unknown").to_owned();
        if needs_reembed(&kind) {
            report.needs_reembed += 1;
            if let Some(w) = manifest.as_mut() {
                write_manifest_line(w, scope, &kind, &key)?;
            }
        }
        *report.by_kind.entry(kind).or_insert(0) += 1;

        if opts.writes_enabled() {
            batch.push(WriteOp::KvPut { key, value });
            if batch.len() >= opts.batch_size {
                flush(dest, scope, &mut batch, &mut report).await?;
            }
        }
    }
    if !batch.is_empty() {
        flush(dest, scope, &mut batch, &mut report).await?;
    }
    if let Some(mut w) = manifest {
        w.flush()?;
    }
    Ok(report)
}

/// Commit one batch and fold the result into `report`.
async fn flush(
    dest: &dyn StoragePort,
    scope: &Scope,
    batch: &mut Vec<WriteOp>,
    report: &mut ScopeReport,
) -> Result<(), MigrateError> {
    let n = batch.len() as u64;
    dest.atomic_write(scope, batch).await?;
    report.written += n;
    report.batches += 1;
    batch.clear();
    Ok(())
}

fn open_manifest(path: &Path) -> Result<std::io::BufWriter<std::fs::File>, std::io::Error> {
    // Truncate: a manifest is the backlog of THIS run, and appending to a stale
    // one would hand the operator a re-embed list that no longer matches.
    Ok(std::io::BufWriter::new(std::fs::File::create(path)?))
}

fn write_manifest_line(
    w: &mut std::io::BufWriter<std::fs::File>,
    scope: &Scope,
    kind: &str,
    key: &[u8],
) -> Result<(), std::io::Error> {
    let line = serde_json::json!({
        "scope": scope.as_str(),
        "kind": kind,
        "key": String::from_utf8_lossy(key),
    });
    writeln!(w, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend that hands back the cursor it was given makes the enumeration
    /// loop spin forever. An operator tool that hangs is worse than one that
    /// stops with a diagnosis.
    #[test]
    fn a_repeated_cursor_is_treated_as_a_stall() {
        assert!(pagination_stalled(Some("abc"), "abc"));
        assert!(pagination_stalled(None, ""));
    }

    #[test]
    fn an_advancing_cursor_is_not_a_stall() {
        assert!(!pagination_stalled(None, "abc"));
        assert!(!pagination_stalled(Some("abc"), "def"));
    }

    #[test]
    fn skipped_sums_every_class() {
        let r = ScopeReport {
            skipped_closed_valid: 2,
            skipped_closed_sys: 3,
            skipped_foreign_key: 4,
            ..ScopeReport::default()
        };
        assert_eq!(r.skipped(), 9);
    }

    #[test]
    fn ack_error_message_points_at_the_flag() {
        assert!(MigrateError::LossyNotAcknowledged.to_string().contains("--acknowledge-lossy"));
    }
}
