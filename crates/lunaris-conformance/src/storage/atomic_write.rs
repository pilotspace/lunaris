//! Plan 05-02 D-17 — `atomic_write` conformance assertions.
//!
//! Two functions:
//!
//! - [`all_or_nothing`] writes 3 ops, then asserts each key is readable
//!   via `read_as_of`. Backends that drop or partially-commit any of the
//!   batch fail this test.
//! - [`lsn_monotonic`] writes 5 single-op batches and asserts the
//!   returned `Lsn` strictly increases. `Lsn` derives `Ord` (lex on
//!   `(wall_ms, counter)`), so we can compare directly.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::{Lsn, WriteOp};
use lunaris_core::storage::StoragePort;

/// Asserts that an atomic_write of N ops either commits ALL or NONE.
///
/// Happy-path shape: write 3 keys in one atomic_write, then assert all
/// 3 are readable via `read_as_of` at a "now" timestamp far in the
/// future (so any backend's snapshot semantics include the just-committed
/// row regardless of HLC clock skew).
pub async fn all_or_nothing(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    let keys: [&[u8]; 3] = [
        b"conformance:aw:k1",
        b"conformance:aw:k2",
        b"conformance:aw:k3",
    ];
    let ops: Vec<WriteOp> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| WriteOp::KvPut {
            key: k.to_vec(),
            value: format!("v{i}").into_bytes(),
        })
        .collect();

    let lsn = storage.atomic_write(&ops).await?;

    // Read at a generous "now" — backends with snapshot pinning need
    // a timestamp that comfortably overlaps the commit's HLC.
    let now = Hlc {
        wall_ms: u64::MAX / 2,
        counter: 0,
        node_id: 0,
    };
    for k in keys.iter() {
        let row = storage.read_as_of(k, now).await?;
        anyhow::ensure!(
            row.is_some(),
            "atomic_write::all_or_nothing: key {:?} missing after commit (lsn={:?})",
            std::str::from_utf8(k).unwrap_or("<non-utf8>"),
            lsn,
        );
    }
    Ok(())
}

/// Asserts that returned `Lsn` strictly increases across sequential
/// `atomic_write` calls.
///
/// `Lsn` derives `PartialOrd + Ord` (`#[derive(... Ord)]` on a
/// `{ wall_ms: u64, counter: u32 }` lex tuple per
/// `crates/lunaris-core/src/storage/types.rs:16`), so direct `>`
/// comparison is correct.
pub async fn lsn_monotonic(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    let mut prev: Option<Lsn> = None;
    for i in 0..5u32 {
        let key = format!("conformance:lsn:{i:03}").into_bytes();
        let ops = vec![WriteOp::KvPut {
            key,
            value: format!("v{i}").into_bytes(),
        }];
        let lsn = storage.atomic_write(&ops).await?;
        if let Some(p) = prev {
            anyhow::ensure!(
                lsn > p,
                "atomic_write::lsn_monotonic: returned Lsn {lsn:?} not strictly greater than prior {p:?}",
            );
        }
        prev = Some(lsn);
    }
    Ok(())
}
