//! Plan 05-02 D-17 — `read_as_of` conformance.
//!
//! Asserts the basic temporal-read contract: a fresh `read_as_of` at
//! `Hlc::ZERO` (before any write) returns `None`; a `read_as_of` after a
//! commit at a generous "now" returns `Some(Row)` with the committed
//! payload.
//!
//! ## Two different questions (0.6.2 task 9)
//!
//! `StorageCapabilities::bi_temporal_native` answers "are temporal reads a
//! NATIVE backend feature" — Postgres reports `false` because it emulates
//! the snapshot with `valid_from`/`valid_to`/`sys_*` predicates, and Moon
//! reports `false` too. It does NOT answer "can this backend answer a
//! historical read at all", which is
//! [`StoragePort::supports_historical_kv_reads`]: `true` for Postgres and
//! the embedded SQLite backend, `false` for Moon (plain hashes, no version
//! chain). Conflating the two is what let the Moon gap hide behind a
//! conformance suite that never asked.
//!
//! [`snapshot`] pins the latest-state contract every backend must meet;
//! [`historical_pin_is_explicit`] pins the historical one, in BOTH
//! directions, so a backend can never quietly answer a time-travel query
//! with present-time data.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::WriteOp;

/// A pin unambiguously in the past — 1970-01-01T00:00:01Z. Older than any
/// possible write, so a backend with a real version chain must answer
/// "absent" and a backend without one must answer "cannot".
fn historical_pin() -> Hlc {
    Hlc::from_parts(1_000, 0, 0)
}

/// Wall-clock "now" as an `Hlc` — the latest-state read every production
/// call site issues.
fn now_pin() -> Hlc {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Hlc::from_parts(ms, 0, 0)
}

pub async fn snapshot(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    let key: Vec<u8> = b"conformance:read_as_of:k1".to_vec();
    let value: Vec<u8> = b"snapshot-test-payload".to_vec();

    // Pre-write read must return None for a key that has never been
    // written. (Note: any other test in this suite writing under the same
    // key would invalidate this; the conformance contract assumes a clean
    // slate per-key.)
    //
    // 0.6.2 task 9: probe at "now", not `Hlc::ZERO`. A ZERO pin is a
    // *historical* request, which a backend without a KV version chain
    // (Moon) now correctly refuses — the refusal is asserted by
    // `historical_pin_is_explicit`, and asserting it here too would
    // conflate "the key is absent" with "the backend cannot look back".
    let pre = storage.read_as_of(&lunaris_core::Scope::dev(), &key, now_pin()).await?;
    anyhow::ensure!(
        pre.is_none(),
        "read_as_of::snapshot: expected None at Hlc::ZERO before any write, got {:?}",
        pre,
    );

    storage
        .atomic_write(
            &lunaris_core::Scope::dev(),
            &[WriteOp::KvPut { key: key.clone(), value: value.clone() }],
        )
        .await?;

    let now = Hlc { wall_ms: u64::MAX / 2, counter: 0, node_id: 0 };
    let post =
        storage.read_as_of(&lunaris_core::Scope::dev(), &key, now).await?.ok_or_else(|| {
            anyhow::anyhow!("read_as_of::snapshot: expected Some after commit, got None")
        })?;
    anyhow::ensure!(
        post.value.as_ref() == value.as_slice(),
        "read_as_of::snapshot: payload mismatch — got {} bytes, want {}",
        post.value.len(),
        value.len(),
    );
    Ok(())
}

/// 0.6.2 task 9 — a historical pin must be answered honestly or not at all.
///
/// Writes a row NOW, then reads it at a 1970 pin. Exactly one of two
/// outcomes is conformant, selected by the backend's own declaration:
///
/// * [`StoragePort::supports_historical_kv_reads`] `== true` — the backend
///   has a version chain, so the row it just wrote MUST NOT be visible at a
///   timestamp before it existed. `Ok(None)` (or any row that is not the
///   one just written) is correct; returning the fresh row is a broken
///   snapshot.
/// * `== false` — the backend cannot look back and MUST say so with
///   `StorageError::NotSupported(_)` (which `lunaris-server` maps to
///   `501 not_supported`). `Ok(Some(row))` is the silent-wrongness bug this
///   assertion exists to prevent; a bare `Ok(None)` is equally wrong — it
///   asserts "nothing existed then", a claim the backend cannot make.
///
/// Runs for EVERY backend, gated on nothing. A backend that cannot do
/// as-of reads still has to prove it fails loudly.
pub async fn historical_pin_is_explicit(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    let scope = lunaris_core::Scope::dev();
    let key: Vec<u8> = format!("conformance:read_as_of:historical:{}", ulid::Ulid::new()).into();
    let value: Vec<u8> = b"written-now-not-in-1970".to_vec();

    storage
        .atomic_write(&scope, &[WriteOp::KvPut { key: key.clone(), value: value.clone() }])
        .await?;

    let got = storage.read_as_of(&scope, &key, historical_pin()).await;

    if storage.supports_historical_kv_reads() {
        match got {
            Ok(None) => Ok(()),
            Ok(Some(row)) => anyhow::bail!(
                "read_as_of::historical_pin_is_explicit: backend declares \
                 supports_historical_kv_reads()=true but a 1970 pin returned a {}-byte row — \
                 the snapshot is not honoring the version chain",
                row.value.len(),
            ),
            Err(e) => anyhow::bail!(
                "read_as_of::historical_pin_is_explicit: backend declares \
                 supports_historical_kv_reads()=true but refused the historical read: {e}",
            ),
        }
    } else {
        match got {
            Err(StorageError::NotSupported(_)) => Ok(()),
            Err(e) => anyhow::bail!(
                "read_as_of::historical_pin_is_explicit: a backend without historical KV reads \
                 must refuse with StorageError::NotSupported (→ HTTP 501 not_supported), \
                 got a different error: {e}",
            ),
            other => anyhow::bail!(
                "read_as_of::historical_pin_is_explicit: backend declares \
                 supports_historical_kv_reads()=false yet ANSWERED a 1970 pin with {other:?}. \
                 Returning present-time data (or a bare Ok(None), which claims nothing existed \
                 then) for a snapshot the backend cannot reconstruct is the silent bi-temporal \
                 violation this conformance test exists to catch — refuse with \
                 StorageError::NotSupported instead",
            ),
        }
    }
}
