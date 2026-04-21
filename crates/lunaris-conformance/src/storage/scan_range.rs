//! Plan 05-02 D-17 — `scan_range` conformance.
//!
//! Verbatim mirror of
//! `crash_recovery.rs::count_primitives_via_scan_range` (Plan 04-03
//! Shared Pattern, PATTERNS.md exact match): seed a known prefix with N
//! KvPut rows, walk the resulting `BoxStream<'_, Result<(Bytes, Bytes),
//! StorageError>>` to completion, and assert the stream yielded exactly N
//! items (no panics on mid-stream errors).

#![forbid(unsafe_code)]

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use lunaris_core::error::StorageError;
use lunaris_core::storage::types::WriteOp;
use lunaris_core::storage::StoragePort;

/// Number of seeded rows under the `scan_range` prefix. 7 is small
/// enough to stay deterministic AND large enough to catch off-by-one
/// stream-walk bugs.
const SEEDED_ROWS: usize = 7;

pub async fn prefix(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    let prefix: &[u8] = b"conformance:scan_range:";

    // Seed each row in its own atomic_write so that order is well-defined
    // (the conformance contract is about scan completeness, not LSN
    // packing).
    for i in 0..SEEDED_ROWS {
        let key = format!("conformance:scan_range:{i:03}").into_bytes();
        storage
            .atomic_write(&[WriteOp::KvPut {
                key,
                value: format!("v{i}").into_bytes(),
            }])
            .await?;
    }

    let count = count_under_prefix(storage, prefix).await?;
    anyhow::ensure!(
        count as usize == SEEDED_ROWS,
        "scan_range::prefix: expected {SEEDED_ROWS} rows under {:?}, got {count}",
        std::str::from_utf8(prefix).unwrap_or("<non-utf8>"),
    );
    Ok(())
}

/// Walks the `scan_range` stream to completion, summing successful items
/// and bailing on any mid-stream `Err`.
///
/// Mirrors `crash_recovery.rs:202-217` shape verbatim — the only delta is
/// returning a `Result` instead of `panic!()` so the failure surfaces
/// through `anyhow` rather than a runtime panic.
async fn count_under_prefix(
    storage: &Arc<dyn StoragePort>,
    prefix: &[u8],
) -> anyhow::Result<u64> {
    let mut stream = storage.scan_range(prefix, None).await?;
    let mut total = 0u64;
    while let Some(item) = stream.next().await {
        match item {
            Ok::<(Bytes, Bytes), StorageError>(_) => total += 1,
            Err(e) => anyhow::bail!("scan_range mid-stream error: {e}"),
        }
    }
    Ok(total)
}
