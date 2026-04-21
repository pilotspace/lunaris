//! Plan 05-02 D-17 — `read_as_of` conformance.
//!
//! Asserts the basic temporal-read contract: a fresh `read_as_of` at
//! `Hlc::ZERO` (before any write) returns `None`; a `read_as_of` after a
//! commit at a generous "now" returns `Some(Row)` with the committed
//! payload.
//!
//! Per CONTEXT.md D-12: Postgres reports `bi_temporal_native = false`
//! (it emulates AS_OF via `valid_from` / `valid_to` columns) and Moon
//! reports `true`. The assertions in this test hold for BOTH semantics
//! — pre-write reads must be empty regardless of bi-temporal native
//! support.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::hlc::Hlc;
use lunaris_core::storage::types::WriteOp;
use lunaris_core::storage::StoragePort;

pub async fn snapshot(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    let key: Vec<u8> = b"conformance:read_as_of:k1".to_vec();
    let value: Vec<u8> = b"snapshot-test-payload".to_vec();

    // Pre-write read at Hlc::ZERO must return None for a key that has
    // never been written. (Note: any other test in this suite writing
    // under the same key would invalidate this; the conformance contract
    // assumes a clean slate per-key.)
    let pre = storage.read_as_of(&key, Hlc::ZERO).await?;
    anyhow::ensure!(
        pre.is_none(),
        "read_as_of::snapshot: expected None at Hlc::ZERO before any write, got {:?}",
        pre,
    );

    storage
        .atomic_write(&[WriteOp::KvPut {
            key: key.clone(),
            value: value.clone(),
        }])
        .await?;

    let now = Hlc {
        wall_ms: u64::MAX / 2,
        counter: 0,
        node_id: 0,
    };
    let post = storage
        .read_as_of(&key, now)
        .await?
        .ok_or_else(|| anyhow::anyhow!("read_as_of::snapshot: expected Some after commit, got None"))?;
    anyhow::ensure!(
        post.value.as_ref() == value.as_slice(),
        "read_as_of::snapshot: payload mismatch — got {} bytes, want {}",
        post.value.len(),
        value.len(),
    );
    Ok(())
}
