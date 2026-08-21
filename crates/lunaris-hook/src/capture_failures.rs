//! W4.16 — make a dropped capture discoverable.
//!
//! The headline feature fails silently. On 2026-08-21 Moon 6381 refused
//! **every** write for roughly fifty minutes (`MOONERR diskfull`) and nothing
//! anywhere said so. The whole chain swallows it: Moon rejects the write;
//! `contextd` reports it through `tracing`, whose writer is pinned to stderr
//! while the running daemon has both fd 1 and fd 2 on `/dev/null`; the codex
//! hook adapter discards `contextd_request`'s return value and returns 0
//! unconditionally; and there is no hook-side log file at all. A capture that
//! silently vanished is byte-for-byte indistinguishable from one that
//! succeeded.
//!
//! Note that a log sink alone would not have fixed it. There are two
//! independent causes — the `/dev/null` fds AND the fact that the failure was
//! reported at `debug!` while `contextd`'s default filter is `warn` — so the
//! message was invisible twice over.
//!
//! What must NOT change: the hook stays quiet and stays zero-exit. A hook that
//! breaks the user's session over a storage hiccup is worse than the bug. So
//! the failure becomes *discoverable*, not fatal — an append-only file that
//! outlives the process and the daemon's fds, which an operator (or the
//! SessionStart digest) can read after the fact.
//!
//! Bounded on purpose: a store that refuses every write would otherwise append
//! a line per capture forever. At [`MAX_LOG_BYTES`] the file rotates to a
//! single `.1` sibling, so the record costs at most two bounded files and the
//! most recent failures always survive.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Rotate once the live log passes this size. Two files at this bound is the
/// entire on-disk cost of the feature.
pub const MAX_LOG_BYTES: u64 = 64 * 1024;

/// `LUNARIS_CAPTURE_FAILURE_LOG` override, else
/// `~/.lunaris/logs/capture-failures.log` — a sibling of the `moon-6381*.log`
/// files an operator already looks at.
///
/// Returns `None` only when there is no home directory to anchor to, in which
/// case there is nowhere durable to write and the caller degrades to the
/// `tracing` line alone.
pub fn default_log_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LUNARIS_CAPTURE_FAILURE_LOG") {
        return Some(PathBuf::from(p));
    }
    dirs::home_dir().map(|h| h.join(".lunaris").join("logs").join("capture-failures.log"))
}

/// Append one failure record. Testable body: the path and timestamp are
/// arguments, so a test points it at a tempdir without mutating process env
/// (`set_var` is `unsafe` in edition 2024, and a sibling test reading the same
/// variable indirectly is how `a_maintenance_compact` raced).
pub fn record_at(path: &Path, stamp: &str, err: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(md) = std::fs::metadata(path)
        && md.len() > MAX_LOG_BYTES
    {
        // Single-generation rotation; the previous `.1` is discarded.
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    // One record per line — newlines in the error would forge extra records.
    writeln!(f, "{stamp}\t{}", err.replace(['\n', '\r'], " "))
}

/// Best-effort record at the default location.
///
/// Deliberately infallible: this runs on the capture failure path, and a
/// failure to report a failure must not escalate. If the write cannot happen
/// the `tracing` line at the call site remains the only trace, which is the
/// status quo this module improves on rather than a regression.
pub fn record(err: &str) {
    let Some(path) = default_log_path() else {
        return;
    };
    let stamp = chrono::Utc::now().to_rfc3339();
    let _ = record_at(&path, &stamp, err);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_survives_as_a_readable_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("logs").join("capture-failures.log");
        record_at(&path, "2026-08-21T00:00:00Z", "MOONERR diskfull").expect("record");
        let body = std::fs::read_to_string(&path).expect("read back");
        assert!(body.contains("MOONERR diskfull"), "the error text must survive: {body:?}");
        assert!(body.contains("2026-08-21T00:00:00Z"), "the timestamp must survive: {body:?}");
    }

    #[test]
    fn a_multiline_error_stays_one_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture-failures.log");
        record_at(&path, "t", "line one\nline two").expect("record");
        let body = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(
            body.lines().count(),
            1,
            "an embedded newline must not forge a second record: {body:?}"
        );
    }

    #[test]
    fn the_log_rotates_instead_of_growing_without_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture-failures.log");
        let big = "x".repeat(MAX_LOG_BYTES as usize + 1);
        std::fs::write(&path, &big).expect("seed an oversized log");
        record_at(&path, "t", "after rotation").expect("record");

        let live = std::fs::read_to_string(&path).expect("read live");
        assert!(
            live.len() < MAX_LOG_BYTES as usize,
            "the live log must have been rotated away, got {} bytes",
            live.len()
        );
        assert!(live.contains("after rotation"), "the newest record must be in the live file");
        assert!(
            path.with_extension("log.1").exists(),
            "the rotated generation must be kept, not deleted"
        );
    }
}
