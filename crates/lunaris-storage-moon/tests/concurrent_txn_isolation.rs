//! Live-Moon discriminating test for the cross-store-transaction multiplexing
//! hazard found during the native-MiniMax graph A/B (2026-07).
//!
//! `atomic_write` wraps its per-op fan-out in `TXN.BEGIN … TXN.COMMIT`. Moon
//! tracks the active cross-store transaction PER PHYSICAL CONNECTION
//! (`conn_state.active_cross_txn`), and `MoonClient` shares ONE
//! `redis::aio::MultiplexedConnection` across every clone. So when two
//! `atomic_write`s run concurrently on the same `MoonStorage`, their two
//! `TXN.BEGIN`s multiplex onto the same socket and the second one gets
//! `ERR already in a cross-store transaction` — the first-observed failure was
//! LongMemEval q89 under `LUNARIS_EVAL_LME_INGEST_CONCURRENCY=2`, where a
//! per-session ingest lost the race and the whole question failed to score.
//!
//! This test fans out N concurrent `atomic_write`s (distinct KV keys, so there
//! is NO key contention — the only thing under test is txn serialization) and
//! asserts every one succeeds. Without the per-connection serialization guard
//! in `atomic_write`, ~N-1 of them fail with the collision error. Gated behind
//! `moon-it` + a reachable `MOON_URL`, like the other live-Moon tests.
//!
//! Run: `cargo test -p lunaris-storage-moon --features moon-it \
//!        --test concurrent_txn_isolation` with `MOON_URL` at a live Moon.

#![cfg(feature = "moon-it")]

use futures::stream::{self, StreamExt};
use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::WriteOp;
use lunaris_storage_moon::MoonStorage;
use std::sync::Arc;

fn url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".to_string())
}

async fn connect_or_skip() -> Option<MoonStorage> {
    match MoonStorage::connect(&url()).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("MOON_URL not reachable ({e}); SKIP");
            None
        }
    }
}

fn kv_put(i: usize) -> Vec<WriteOp> {
    vec![WriteOp::KvPut {
        key: format!("txniso:k{i}").into_bytes(),
        value: format!("v{i}").into_bytes(),
    }]
}

/// N concurrent `atomic_write`s on ONE `MoonStorage` must all commit — none may
/// return `already in a cross-store transaction`. This is the regression guard
/// for the shared-`MultiplexedConnection` txn collision that `atomic_write`'s
/// per-connection serialization guard fixes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_atomic_writes_never_collide_on_shared_cross_store_txn() {
    let Some(storage) = connect_or_skip().await else { return };
    let scope = Scope::new("txniso").expect("valid scope");

    // Warm up the scope's indices with ONE serial write so the concurrent burst
    // below exercises only the TXN.BEGIN/COMMIT path, not lazy FT.CREATE.
    storage.atomic_write(&scope, &kv_put(0)).await.expect("warmup write");

    const N: usize = 16;
    let storage = Arc::new(storage);
    let results: Vec<Result<_, _>> = stream::iter(1..=N)
        .map(|i| {
            let storage = Arc::clone(&storage);
            let scope = scope.clone();
            async move { storage.atomic_write(&scope, &kv_put(i)).await }
        })
        .buffer_unordered(N)
        .collect()
        .await;

    let errs: Vec<String> =
        results.iter().filter_map(|r| r.as_ref().err().map(|e| e.to_string())).collect();
    assert!(
        errs.is_empty(),
        "{}/{N} concurrent atomic_writes collided on the shared cross-store txn: {errs:?}",
        errs.len()
    );
}
